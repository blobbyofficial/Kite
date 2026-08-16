//! Menus, media pool, inspector, transport and dialogs.

use crate::app::{App, PROXY_HEIGHTS};
use crate::export::Quality;
use crate::project::{timecode, ClipSource, ImportState, MediaId, TextAlign, TrackKind};
use crate::project::ColorAdjust;
use crate::theme;
use egui::{Align, Align2, Color32, Context, CornerRadius, Layout, Pos2, Rect, Sense, Vec2};

pub fn top_bar(app: &mut App, ctx: &Context) {
    egui::TopBottomPanel::top("menu")
        .frame(egui::Frame::NONE.fill(theme::PANEL).inner_margin(egui::Margin::symmetric(6, 4)))
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("New project        Ctrl+N").clicked() {
                        app.new_project();
                        ui.close();
                    }
                    if ui.button("Open…              Ctrl+O").clicked() {
                        app.open();
                        ui.close();
                    }
                    if ui.button("Save               Ctrl+S").clicked() {
                        app.save(false);
                        ui.close();
                    }
                    if ui.button("Save as…     Ctrl+Shift+S").clicked() {
                        app.save(true);
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Import media…      Ctrl+I").clicked() {
                        app.import_dialog();
                        ui.close();
                    }
                    if ui.button("Export video…      Ctrl+E").clicked() {
                        app.show_export = true;
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Quit").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
                ui.menu_button("Edit", |ui| {
                    if ui.add_enabled(app.history.can_undo(), egui::Button::new("Undo   Ctrl+Z")).clicked() {
                        app.undo();
                        ui.close();
                    }
                    if ui.add_enabled(app.history.can_redo(), egui::Button::new("Redo   Ctrl+Shift+Z")).clicked() {
                        app.redo();
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Split at playhead   S").clicked() {
                        app.split_at_playhead();
                        ui.close();
                    }
                    if ui.button("Delete            Del").clicked() {
                        app.delete_selected(false);
                        ui.close();
                    }
                    if ui.button("Ripple delete Shift+Del").clicked() {
                        app.delete_selected(true);
                        ui.close();
                    }
                    if ui.button("Duplicate      Ctrl+D").clicked() {
                        app.duplicate_selected();
                        ui.close();
                    }
                    if ui.button("Select all     Ctrl+A").clicked() {
                        app.select_all();
                        ui.close();
                    }
                });
                ui.menu_button("Project", |ui| {
                    ui.label("Sequence");
                    let mut s = app.project.settings;
                    let mut changed = false;
                    egui::ComboBox::from_label("Resolution")
                        .selected_text(format!("{}×{}", s.width, s.height))
                        .show_ui(ui, |ui| {
                            for (w, h, name) in [
                                (1920u32, 1080u32, "1920×1080  HD"),
                                (2560, 1440, "2560×1440  QHD"),
                                (3840, 2160, "3840×2160  4K"),
                                (1080, 1920, "1080×1920  Vertical"),
                                (1080, 1080, "1080×1080  Square"),
                            ] {
                                if ui.selectable_label(s.width == w && s.height == h, name).clicked() {
                                    s.width = w;
                                    s.height = h;
                                    changed = true;
                                }
                            }
                        });
                    egui::ComboBox::from_label("Frame rate")
                        .selected_text(format!("{} fps", s.fps))
                        .show_ui(ui, |ui| {
                            for f in [24u32, 25, 30, 50, 60] {
                                if ui.selectable_label(s.fps == f, format!("{f} fps")).clicked() {
                                    s.fps = f;
                                    changed = true;
                                }
                            }
                        });
                    if changed {
                        app.set_sequence(s);
                    }
                    ui.separator();
                    ui.label("Playback proxy quality");
                    let mut ph = app.proxy_height;
                    for (h, label) in PROXY_HEIGHTS {
                        if ui.selectable_label(ph == h, label).clicked() {
                            ph = h;
                        }
                    }
                    if ph != app.proxy_height {
                        app.set_proxy_height(ph);
                    }
                });
                ui.menu_button("Help", |ui| {
                    if ui.button("Keyboard shortcuts").clicked() {
                        app.show_shortcuts = true;
                        ui.close();
                    }
                    ui.separator();
                    ui.label(format!("Kite {}", env!("CARGO_PKG_VERSION")));
                    ui.label(format!("Audio: {}", app.audio.device_name));
                });

                ui.separator();
                if ui.button("＋ Import").on_hover_text("Ctrl+I").clicked() {
                    app.import_dialog();
                }
                if ui.button("⏵ Export").on_hover_text("Ctrl+E").clicked() {
                    app.show_export = true;
                }

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let name = app.project.name.clone();
                    let dirty = if app.dirty { " •" } else { "" };
                    ui.label(egui::RichText::new(format!("{name}{dirty}")).color(theme::TEXT_DIM));
                });
            });
        });
}

pub fn media_panel(app: &mut App, ctx: &Context) {
    egui::SidePanel::left("media")
        .resizable(true)
        .default_width(230.0)
        .width_range(170.0..=400.0)
        .frame(egui::Frame::NONE.fill(theme::PANEL).inner_margin(egui::Margin::same(8)))
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("MEDIA").font(theme::mono(11.0)).color(theme::TEXT_DIM));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("＋").on_hover_text("Import media").clicked() {
                        app.import_dialog();
                    }
                });
            });
            ui.add_space(4.0);

            if app.project.media.is_empty() {
                ui.add_space(20.0);
                ui.vertical_centered(|ui| {
                    ui.label(egui::RichText::new("Drop video files here").color(theme::TEXT_DIM));
                    ui.add_space(6.0);
                    if ui.button("Import media…").clicked() {
                        app.import_dialog();
                    }
                });
                return;
            }

            let mut to_insert: Option<MediaId> = None;
            let mut to_remove: Option<MediaId> = None;
            egui::ScrollArea::vertical().show(ui, |ui| {
                let items: Vec<_> = app.project.media.iter().map(|m| {
                    (m.id, m.name.clone(), m.state, m.duration, m.has_video, m.has_audio, m.error.clone())
                }).collect();
                for (id, name, state, duration, has_v, has_a, err) in items {
                    let selected = app.selected_media == Some(id);
                    let resp = ui.allocate_response(
                        Vec2::new(ui.available_width(), 40.0),
                        Sense::click(),
                    );
                    let r = resp.rect;
                    let bg = if selected {
                        theme::ACCENT_DIM
                    } else if resp.hovered() {
                        theme::PANEL_HI
                    } else {
                        Color32::TRANSPARENT
                    };
                    ui.painter().rect_filled(r, CornerRadius::same(3), bg);

                    let icon = if has_v { "▣" } else if has_a { "♪" } else { "?" };
                    ui.painter().text(
                        Pos2::new(r.left() + 8.0, r.center().y),
                        Align2::LEFT_CENTER,
                        icon,
                        theme::ui_font(13.0),
                        theme::TEXT_DIM,
                    );
                    let text_rect = Rect::from_min_max(
                        Pos2::new(r.left() + 26.0, r.top() + 3.0),
                        Pos2::new(r.right() - 4.0, r.bottom() - 3.0),
                    );
                    ui.painter().with_clip_rect(text_rect).text(
                        text_rect.min,
                        Align2::LEFT_TOP,
                        &name,
                        theme::ui_font(11.5),
                        theme::TEXT,
                    );

                    let sub = match state {
                        ImportState::Queued => "queued…".to_string(),
                        ImportState::Probing => "reading…".to_string(),
                        ImportState::Building(p) => format!("preparing  {p}%"),
                        ImportState::Ready => {
                            let secs = duration.max(0.0);
                            format!("{:02}:{:05.2}", (secs / 60.0) as u32, secs % 60.0)
                        }
                        ImportState::Failed => err.unwrap_or_else(|| "failed".into()),
                    };
                    let sub_col = match state {
                        ImportState::Failed => theme::BAD,
                        ImportState::Ready => theme::TEXT_DIM,
                        _ => theme::WARN,
                    };
                    ui.painter().with_clip_rect(text_rect).text(
                        Pos2::new(text_rect.left(), text_rect.bottom()),
                        Align2::LEFT_BOTTOM,
                        sub,
                        theme::mono(9.5),
                        sub_col,
                    );

                    if let ImportState::Building(p) = state {
                        let bar = Rect::from_min_max(
                            Pos2::new(r.left() + 26.0, r.bottom() - 2.0),
                            Pos2::new(r.left() + 26.0 + (r.width() - 30.0) * p as f32 / 100.0, r.bottom()),
                        );
                        ui.painter().rect_filled(bar, CornerRadius::ZERO, theme::ACCENT);
                    }

                    if resp.clicked() {
                        app.selected_media = Some(id);
                    }
                    if resp.double_clicked() {
                        to_insert = Some(id);
                    }
                    resp.context_menu(|ui| {
                        if ui.button("Add to timeline").clicked() {
                            to_insert = Some(id);
                            ui.close();
                        }
                        if ui.button("Remove from project").clicked() {
                            to_remove = Some(id);
                            ui.close();
                        }
                    });
                }
            });

            ui.add_space(6.0);
            let can_add = app.selected_media.is_some();
            if ui
                .add_enabled(can_add, egui::Button::new("Add to timeline").min_size(Vec2::new(ui.available_width(), 24.0)))
                .on_hover_text("Or double-click an item")
                .clicked()
            {
                to_insert = app.selected_media;
            }

            if let Some(id) = to_insert {
                app.insert_media(id);
            }
            if let Some(id) = to_remove {
                app.remove_media(id);
            }
        });
}

pub fn inspector(app: &mut App, ctx: &Context) {
    egui::SidePanel::right("inspector")
        .resizable(true)
        .default_width(250.0)
        .width_range(200.0..=420.0)
        .frame(egui::Frame::NONE.fill(theme::PANEL).inner_margin(egui::Margin::same(8)))
        .show(ctx, |ui| {
            ui.label(egui::RichText::new("INSPECTOR").font(theme::mono(11.0)).color(theme::TEXT_DIM));
            ui.add_space(6.0);

            let sel = app.project.selected_ids();
            if sel.is_empty() {
                ui.label(egui::RichText::new("Select a clip on the timeline").color(theme::TEXT_DIM));
                return;
            }
            if sel.len() > 1 {
                ui.label(format!("{} clips selected", sel.len()));
                ui.add_space(6.0);
            }
            let cid = sel[0];
            let Some((track, clip)) = app.project.clip(cid) else { return };
            let kind = track.kind;
            let fps = app.project.settings.fps;
            let mut c = clip.clone();
            let mut changed = false;
            let mut speed_change: Option<f32> = None;
            let mut crossfade: Option<i64> = None;

            let title = match &c.source {
                ClipSource::Media(m) => app.project.media(*m).map(|m| m.name.clone()).unwrap_or_default(),
                ClipSource::Text(_) => "Title".into(),
                ClipSource::Color(_) => "Colour card".into(),
            };
            ui.label(egui::RichText::new(title).strong());
            ui.label(
                egui::RichText::new(format!(
                    "{}  →  {}   ({})",
                    timecode(c.start, fps),
                    timecode(c.end(), fps),
                    timecode(c.len, fps)
                ))
                .font(theme::mono(10.0))
                .color(theme::TEXT_DIM),
            );
            ui.separator();

            egui::ScrollArea::vertical().show(ui, |ui| {
                if let ClipSource::Text(t) = &mut c.source {
                    ui.label("Text");
                    if ui.add(egui::TextEdit::multiline(&mut t.text).desired_rows(2)).changed() {
                        changed = true;
                    }
                    changed |= ui.add(egui::Slider::new(&mut t.size, 0.02..=0.35).text("Size")).changed();
                    ui.horizontal(|ui| {
                        ui.label("Align");
                        for (a, l) in [(TextAlign::Left, "L"), (TextAlign::Center, "C"), (TextAlign::Right, "R")] {
                            if ui.selectable_label(t.align == a, l).clicked() {
                                t.align = a;
                                changed = true;
                            }
                        }
                    });
                    changed |= ui.add(egui::Slider::new(&mut t.x, 0.0..=1.0).text("X")).changed();
                    changed |= ui.add(egui::Slider::new(&mut t.y, 0.0..=1.0).text("Y")).changed();
                    let mut col = Color32::from_rgba_unmultiplied(t.color[0], t.color[1], t.color[2], t.color[3]);
                    if ui.color_edit_button_srgba(&mut col).changed() {
                        t.color = [col.r(), col.g(), col.b(), col.a()];
                        changed = true;
                    }
                    changed |= ui.checkbox(&mut t.shadow, "Drop shadow").changed();
                    changed |= ui.checkbox(&mut t.box_bg, "Background box").changed();
                    ui.separator();
                }

                if kind == TrackKind::Video {
                    ui.label("Picture");
                    changed |= ui.add(egui::Slider::new(&mut c.opacity, 0.0..=1.0).text("Opacity")).changed();
                    changed |= ui.add(egui::Slider::new(&mut c.scale, 0.05..=4.0).logarithmic(true).text("Scale")).changed();
                    changed |= ui.add(egui::Slider::new(&mut c.pos_x, -1.0..=1.0).text("Position X")).changed();
                    changed |= ui.add(egui::Slider::new(&mut c.pos_y, -1.0..=1.0).text("Position Y")).changed();
                    if ui.button("Reset transform").clicked() {
                        c.scale = 1.0;
                        c.pos_x = 0.0;
                        c.pos_y = 0.0;
                        changed = true;
                    }
                    ui.separator();
                }

                let has_audio = c
                    .media_id()
                    .and_then(|m| app.project.media(m).map(|m| m.has_audio))
                    .unwrap_or(false);
                if has_audio {
                    ui.label("Audio");
                    let mut db = if c.volume <= 0.0001 { -60.0 } else { 20.0 * c.volume.log10() };
                    if ui.add(egui::Slider::new(&mut db, -60.0..=12.0).text("Volume dB")).changed() {
                        c.volume = if db <= -59.9 { 0.0 } else { 10f32.powf(db / 20.0) };
                        changed = true;
                    }
                    ui.separator();
                }

                if kind == TrackKind::Video {
                    ui.label("Colour");
                    changed |= ui
                        .add(egui::Slider::new(&mut c.color.brightness, -0.5..=0.5).text("Brightness"))
                        .changed();
                    changed |= ui
                        .add(egui::Slider::new(&mut c.color.contrast, 0.0..=2.5).text("Contrast"))
                        .changed();
                    changed |= ui
                        .add(egui::Slider::new(&mut c.color.saturation, 0.0..=2.5).text("Saturation"))
                        .changed();
                    ui.horizontal(|ui| {
                        if ui.small_button("Punchy").clicked() {
                            c.color = ColorAdjust { brightness: 0.02, contrast: 1.15, saturation: 1.18 };
                            changed = true;
                        }
                        if ui.small_button("Flat").clicked() {
                            c.color = ColorAdjust { brightness: 0.0, contrast: 0.88, saturation: 0.9 };
                            changed = true;
                        }
                        if ui.small_button("Mono").clicked() {
                            c.color = ColorAdjust { brightness: 0.0, contrast: 1.05, saturation: 0.0 };
                            changed = true;
                        }
                        if ui.small_button("Reset").clicked() {
                            c.color = ColorAdjust::default();
                            changed = true;
                        }
                    });
                    ui.separator();
                }

                ui.label("Speed");
                let mut speed = c.speed;
                let speed_resp = ui.add(
                    egui::Slider::new(&mut speed, 0.25..=4.0)
                        .logarithmic(true)
                        .text("×"),
                );
                ui.horizontal(|ui| {
                    for (label, v) in [("0.5×", 0.5f32), ("1×", 1.0), ("2×", 2.0), ("4×", 4.0)] {
                        if ui.small_button(label).clicked() {
                            speed = v;
                        }
                    }
                });
                if speed_resp.changed() || (speed - c.speed).abs() > 1e-4 {
                    speed_change = Some(speed);
                }
                ui.separator();

                ui.label("Crossfade from previous clip");
                let mut tr = c.transition_in;
                if ui
                    .add(egui::Slider::new(&mut tr, 0..=(c.len.min(fps as i64 * 3)).max(1)).text("Frames"))
                    .changed()
                {
                    c.transition_in = tr;
                    changed = true;
                }
                if ui.button("Add ½ second dissolve").clicked() {
                    crossfade = Some((fps as i64 / 2).max(1));
                }
                ui.separator();

                ui.label("Fades");
                let max_fade = (c.len / 2).max(1);
                let mut fi = c.fade_in;
                let mut fo = c.fade_out;
                if ui.add(egui::Slider::new(&mut fi, 0..=max_fade).text("In (frames)")).changed() {
                    c.fade_in = fi;
                    changed = true;
                }
                if ui.add(egui::Slider::new(&mut fo, 0..=max_fade).text("Out (frames)")).changed() {
                    c.fade_out = fo;
                    changed = true;
                }
                ui.horizontal(|ui| {
                    if ui.small_button("½s in").clicked() {
                        c.fade_in = (fps as i64 / 2).min(max_fade);
                        changed = true;
                    }
                    if ui.small_button("½s out").clicked() {
                        c.fade_out = (fps as i64 / 2).min(max_fade);
                        changed = true;
                    }
                    if ui.small_button("none").clicked() {
                        c.fade_in = 0;
                        c.fade_out = 0;
                        changed = true;
                    }
                });
            });

            if changed {
                app.apply_clip_edit(cid, c);
            }
            if let Some(sp) = speed_change {
                app.set_clip_speed(cid, sp);
            }
            if let Some(fr) = crossfade {
                app.crossfade_selected(fr);
            }
        });
}

pub fn transport(app: &mut App, ctx: &Context) {
    egui::TopBottomPanel::bottom("transport")
        .frame(egui::Frame::NONE.fill(theme::PANEL).inner_margin(egui::Margin::symmetric(8, 5)))
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui.button("⏮").on_hover_text("Start (Home)").clicked() {
                    app.set_playhead(0);
                }
                if ui.button("◀").on_hover_text("Previous frame (←)").clicked() {
                    let p = app.playhead - 1;
                    app.set_playhead(p);
                }
                let label = if app.playing { "⏸" } else { "▶" };
                if ui
                    .add(egui::Button::new(egui::RichText::new(label).size(15.0)).min_size(Vec2::new(44.0, 24.0)))
                    .on_hover_text("Play / pause (Space)")
                    .clicked()
                {
                    app.toggle_play();
                }
                if ui.button("▶").on_hover_text("Next frame (→)").clicked() {
                    let p = app.playhead + 1;
                    app.set_playhead(p);
                }
                if ui.button("⏭").on_hover_text("End (End)").clicked() {
                    let d = app.duration();
                    app.set_playhead(d);
                }

                ui.add_space(10.0);
                ui.label(
                    egui::RichText::new(timecode(app.playhead, app.fps()))
                        .font(theme::mono(15.0))
                        .color(theme::TEXT),
                );
                ui.label(
                    egui::RichText::new(format!("/ {}", timecode(app.duration(), app.fps())))
                        .font(theme::mono(11.0))
                        .color(theme::TEXT_DIM),
                );

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    meters(app, ui);
                });
            });
        });
}

fn meters(app: &App, ui: &mut egui::Ui) {
    let (l, r) = app.audio.peaks();
    let size = Vec2::new(90.0, 7.0);
    for (v, _label) in [(l, "L"), (r, "R")] {
        let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
        ui.painter().rect_filled(rect, CornerRadius::same(2), theme::BG);
        let w = rect.width() * v.clamp(0.0, 1.0);
        let col = if v > 0.97 {
            theme::BAD
        } else if v > 0.8 {
            theme::WARN
        } else {
            theme::GOOD
        };
        let bar = Rect::from_min_size(rect.min, Vec2::new(w, rect.height()));
        ui.painter().rect_filled(bar, CornerRadius::same(2), col);
    }
}

pub fn status_bar(app: &mut App, ctx: &Context, frame_ms: f32) {
    egui::TopBottomPanel::bottom("status")
        .frame(egui::Frame::NONE.fill(theme::PANEL_HI).inner_margin(egui::Margin::symmetric(8, 3)))
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                let (n, bytes) = app.cache.stats();
                ui.label(
                    egui::RichText::new(format!(
                        "{:.1} ms/frame   ·   cache {n} frames / {} MB",
                        frame_ms,
                        bytes / (1024 * 1024)
                    ))
                    .font(theme::mono(10.0))
                    .color(theme::TEXT_DIM),
                );
                if let Some(e) = &app.audio.error {
                    ui.label(egui::RichText::new(format!("· audio unavailable: {e}")).font(theme::mono(10.0)).color(theme::WARN));
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if let Some((msg, at, is_err)) = app.toast.clone() {
                        if at.elapsed().as_secs_f32() < 5.0 {
                            let c = if is_err { theme::BAD } else { theme::GOOD };
                            ui.label(egui::RichText::new(msg).font(theme::mono(10.5)).color(c));
                            ctx.request_repaint_after(std::time::Duration::from_millis(500));
                        } else {
                            app.toast = None;
                        }
                    }
                });
            });
        });
}

pub fn overlays(app: &mut App, ctx: &Context) {
    recovery_prompt(app, ctx);
    export_dialog(app, ctx);
    export_progress(app, ctx);
    shortcuts_window(app, ctx);
}

fn recovery_prompt(app: &mut App, ctx: &Context) {
    if app.recovery.is_none() {
        return;
    }
    egui::Window::new("Recover unsaved work")
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .show(ctx, |ui| {
            ui.set_min_width(380.0);
            ui.label("Kite found a project from a session that ended without saving.");
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.add(egui::Button::new("Recover it").min_size(Vec2::new(110.0, 26.0))).clicked() {
                    app.recover();
                }
                if ui.button("Start fresh").clicked() {
                    app.discard_recovery();
                }
            });
        });
}

fn export_dialog(app: &mut App, ctx: &Context) {
    if !app.show_export {
        return;
    }
    let mut open = true;
    egui::Window::new("Export video")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .show(ctx, |ui| {
            ui.set_min_width(420.0);
            ui.horizontal(|ui| {
                ui.label("File");
                let shown = app.export_settings.path.display().to_string();
                ui.label(egui::RichText::new(elide(&shown, 42)).font(theme::mono(10.5)).color(theme::TEXT_DIM));
                if ui.button("Change…").clicked() {
                    if let Some(p) = rfd::FileDialog::new()
                        .set_title("Export to")
                        .add_filter("MP4 video", &["mp4"])
                        .set_file_name("my-video.mp4")
                        .save_file()
                    {
                        app.export_settings.path = p;
                    }
                }
            });
            ui.separator();

            let mut q = app.export_settings.quality;
            egui::ComboBox::from_label("Quality")
                .selected_text(q.label())
                .show_ui(ui, |ui| {
                    for opt in [Quality::High, Quality::Balanced, Quality::Small] {
                        ui.selectable_value(&mut q, opt, opt.label());
                    }
                });
            app.export_settings.quality = q;

            let mut e = app.export_settings.encoder;
            let encoders = app.encoders.clone();
            egui::ComboBox::from_label("Encoder")
                .selected_text(e.label())
                .show_ui(ui, |ui| {
                    for opt in encoders {
                        ui.selectable_value(&mut e, opt, opt.label());
                    }
                });
            app.export_settings.encoder = e;

            ui.label(
                egui::RichText::new(format!(
                    "{}×{} · {} fps · {}",
                    app.export_settings.width,
                    app.export_settings.height,
                    app.export_settings.fps,
                    timecode(app.duration(), app.fps())
                ))
                .font(theme::mono(10.5))
                .color(theme::TEXT_DIM),
            );
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new("Export always reads your original files, not the playback proxies.")
                    .font(theme::ui_font(10.5))
                    .color(theme::TEXT_DIM),
            );

            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.add(egui::Button::new("Start export").min_size(Vec2::new(120.0, 26.0))).clicked() {
                    app.begin_export();
                }
                if ui.button("Cancel").clicked() {
                    app.show_export = false;
                }
            });
        });
    if !open {
        app.show_export = false;
    }
}

fn export_progress(app: &mut App, ctx: &Context) {
    if app.export_job.is_none() {
        return;
    }
    egui::Window::new("Exporting")
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .show(ctx, |ui| {
            ui.set_min_width(360.0);
            ui.add(egui::ProgressBar::new(app.export_pct / 100.0).show_percentage());
            ui.label(egui::RichText::new(&app.export_note).font(theme::mono(10.5)).color(theme::TEXT_DIM));
            ui.add_space(6.0);
            if ui.button("Cancel").clicked() {
                if let Some(j) = &app.export_job {
                    j.cancel();
                }
            }
        });
    ctx.request_repaint_after(std::time::Duration::from_millis(200));
}

fn shortcuts_window(app: &mut App, ctx: &Context) {
    if !app.show_shortcuts {
        return;
    }
    let mut open = true;
    egui::Window::new("Keyboard shortcuts")
        .open(&mut open)
        .collapsible(false)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .show(ctx, |ui| {
            let rows = [
                ("Space", "Play / pause"),
                ("← →", "Step one frame"),
                ("Shift + ← →", "Step one second"),
                ("Home / End", "Jump to start / end"),
                ("S  or  Ctrl+K", "Split at playhead"),
                ("Del", "Delete selected clip"),
                ("Shift + Del", "Ripple delete (close the gap)"),
                ("Ctrl+C / Ctrl+V", "Copy and paste clips at the playhead"),
                ("Ctrl+T", "Crossfade into the selected clip"),
                ("Ctrl+D", "Duplicate selected clips"),
                ("Drag empty space", "Rubber-band select"),
                ("Shift + scroll", "Scroll tracks vertically"),
                (", / .", "Nudge selection by a frame (Shift: a second)"),
                ("M", "Toggle snapping"),
                ("+ / −", "Zoom timeline"),
                ("Ctrl + scroll", "Zoom around the pointer"),
                ("Shift + scroll", "Scroll the timeline"),
                ("Ctrl+Z / Ctrl+Shift+Z", "Undo / redo"),
                ("Ctrl+A", "Select all clips"),
                ("Ctrl+I", "Import media"),
                ("Ctrl+E", "Export video"),
                ("Ctrl+S / Ctrl+O", "Save / open project"),
            ];
            egui::Grid::new("sc").num_columns(2).spacing(Vec2::new(24.0, 6.0)).show(ui, |ui| {
                for (k, v) in rows {
                    ui.label(egui::RichText::new(k).font(theme::mono(11.0)).color(theme::ACCENT));
                    ui.label(v);
                    ui.end_row();
                }
            });
        });
    app.show_shortcuts = open;
}

fn elide(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let tail: String = s.chars().skip(s.chars().count() - max + 1).collect();
    format!("…{tail}")
}
