//! Menus, media pool, inspector, transport and dialogs.

use crate::app::{App, PROXY_HEIGHTS};
use crate::export::Quality;
use crate::project::{timecode, ClipSource, ImportState, MediaId, RenderStatus, TextAlign, TrackKind};
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
                    if ui.button("Add to render queue   Ctrl+M").clicked() {
                        app.add_to_render_queue();
                        ui.close();
                    }
                    if ui.button("Render queue…").clicked() {
                        app.show_render_queue = true;
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

                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(&app.project.name)
                        .color(theme::TEXT)
                        .size(12.5),
                );
                if app.dirty {
                    ui.label(
                        egui::RichText::new("unsaved")
                            .font(theme::mono(9.5))
                            .color(theme::WARN),
                    );
                }

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add(egui::Button::new("Render").min_size(Vec2::new(74.0, 24.0)))
                        .on_hover_text("Queue this timeline and open the render queue   Ctrl+M")
                        .clicked()
                    {
                        app.add_to_render_queue();
                    }
                    if ui
                        .button("Import media")
                        .on_hover_text("Add footage, music or images   Ctrl+I")
                        .clicked()
                    {
                        app.import_dialog();
                    }
                });
            });
        });
}

/// The media pool: a bin tree above, the contents of the selected bin below.
///
/// Timelines are items in the pool alongside footage, the way Resolve treats them, so a project
/// can hold several sequences and you switch by opening one.
pub fn media_panel(app: &mut App, ctx: &Context) {
    egui::SidePanel::left("media")
        .resizable(true)
        .default_width(268.0)
        .width_range(200.0..=460.0)
        .frame(egui::Frame::NONE.fill(theme::PANEL).inner_margin(egui::Margin::same(8)))
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("MEDIA POOL")
                        .font(theme::mono(10.0))
                        .color(theme::TEXT_FAINT),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("Import").on_hover_text("Add footage   Ctrl+I").clicked() {
                        app.import_dialog();
                    }
                });
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.small_button("New bin").clicked() {
                    let parent = app.active_bin;
                    app.snapshot();
                    let id = app.project.add_bin("New bin", Some(parent));
                    app.active_bin = id;
                }
                if ui.small_button("New timeline").clicked() {
                    app.show_new_timeline = true;
                    app.new_timeline_name = format!("Timeline {}", app.project.timelines.len() + 1);
                }
            });

            ui.add_space(6.0);
            // ---- bin tree ----
            egui::ScrollArea::vertical()
                .id_salt("bins")
                .max_height(112.0)
                .show(ui, |ui| {
                    let root = app.project.root_bin();
                    draw_bin(app, ui, root, 0);
                });

            ui.add_space(4.0);
            ui.separator();

            let bin = app.active_bin;
            let bin_name = app
                .project
                .bins
                .iter()
                .find(|b| b.id == bin)
                .map(|b| b.name.clone())
                .unwrap_or_else(|| "Master".into());
            ui.label(
                egui::RichText::new(bin_name.to_uppercase())
                    .font(theme::mono(9.5))
                    .color(theme::TEXT_FAINT),
            );

            let mut to_insert: Option<MediaId> = None;
            let mut to_remove: Option<MediaId> = None;
            let mut to_open: Option<u64> = None;
            let mut move_to: Option<(MediaId, u64)> = None;
            let bins: Vec<(u64, String)> =
                app.project.bins.iter().map(|b| (b.id, b.name.clone())).collect();

            egui::ScrollArea::vertical().id_salt("pool").show(ui, |ui| {
                // Timelines first: they are what you open, not what you drag in.
                let tls: Vec<(u64, String, usize, i64)> = app
                    .project
                    .timelines
                    .iter()
                    .filter(|t| t.bin == bin)
                    .map(|t| (t.id, t.name.clone(), t.tracks.len(), t.duration()))
                    .collect();
                let active_id = app.project.tl().id;
                for (id, name, ntracks, dur) in tls {
                    let resp =
                        ui.allocate_response(Vec2::new(ui.available_width(), 34.0), Sense::click());
                    let r = resp.rect;
                    let on = id == active_id;
                    let bg = if on {
                        theme::ACCENT_DIM
                    } else if resp.hovered() {
                        theme::PANEL_HI
                    } else {
                        Color32::TRANSPARENT
                    };
                    ui.painter().rect_filled(r, CornerRadius::same(3), bg);
                    let ic = Rect::from_center_size(
                        Pos2::new(r.left() + 15.0, r.center().y),
                        Vec2::new(15.0, 11.0),
                    );
                    ui.painter().rect_filled(ic, CornerRadius::same(2), theme::ACCENT);
                    ui.painter().text(
                        Pos2::new(r.left() + 28.0, r.top() + 3.0),
                        Align2::LEFT_TOP,
                        &name,
                        theme::ui_font(11.5),
                        theme::TEXT,
                    );
                    ui.painter().text(
                        Pos2::new(r.left() + 28.0, r.bottom() - 3.0),
                        Align2::LEFT_BOTTOM,
                        format!("timeline · {ntracks} tracks · {}", timecode(dur, app.fps())),
                        theme::mono(9.0),
                        theme::TEXT_FAINT,
                    );
                    if resp.clicked() || resp.double_clicked() {
                        to_open = Some(id);
                    }
                }

                let items: Vec<_> = app
                    .project
                    .media
                    .iter()
                    .filter(|m| m.bin == bin)
                    .map(|m| {
                        (
                            m.id,
                            m.name.clone(),
                            m.state,
                            m.duration,
                            m.has_video,
                            m.has_audio,
                            m.error.clone(),
                            m.audio_path.is_some(),
                        )
                    })
                    .collect();

                if items.is_empty() && to_open.is_none() {
                    ui.add_space(10.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            egui::RichText::new("Drop footage here")
                                .color(theme::TEXT_FAINT)
                                .size(11.5),
                        );
                    });
                }

                for (id, name, state, duration, has_v, has_a, err, audio_ready) in items {
                    let selected = app.selected_media == Some(id);
                    let resp =
                        ui.allocate_response(Vec2::new(ui.available_width(), 40.0), Sense::click());
                    let r = resp.rect;
                    let bg = if selected {
                        theme::ACCENT_DIM
                    } else if resp.hovered() {
                        theme::PANEL_HI
                    } else {
                        Color32::TRANSPARENT
                    };
                    ui.painter().rect_filled(r, CornerRadius::same(3), bg);

                    let ic = Rect::from_center_size(
                        Pos2::new(r.left() + 15.0, r.center().y),
                        Vec2::new(15.0, 11.0),
                    );
                    let icol = if has_v { theme::VIDEO_CLIP_HI } else { theme::AUDIO_CLIP_HI };
                    ui.painter().rect_filled(ic, CornerRadius::same(2), icol);
                    if has_a && !has_v {
                        ui.painter().line_segment(
                            [
                                Pos2::new(ic.left() + 3.0, ic.center().y),
                                Pos2::new(ic.right() - 3.0, ic.center().y),
                            ],
                            egui::Stroke::new(1.5, theme::WAVE),
                        );
                    }
                    let text_rect = Rect::from_min_max(
                        Pos2::new(r.left() + 28.0, r.top() + 3.0),
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
                        ImportState::Queued | ImportState::Probing => "reading…".to_string(),
                        ImportState::Building(p) => format!("preparing  {p}%"),
                        ImportState::Ready => {
                            let secs = duration.max(0.0);
                            let len =
                                format!("{:02}:{:05.2}", (secs / 60.0) as u32, secs % 60.0);
                            if has_a && !audio_ready {
                                format!("{len}  ·  sound…")
                            } else {
                                len
                            }
                        }
                        ImportState::Failed => err.unwrap_or_else(|| "failed".into()),
                    };
                    let sub_col = match state {
                        ImportState::Failed => theme::BAD,
                        ImportState::Ready => theme::TEXT_FAINT,
                        _ => theme::WARN,
                    };
                    ui.painter().with_clip_rect(text_rect).text(
                        Pos2::new(text_rect.left(), text_rect.bottom()),
                        Align2::LEFT_BOTTOM,
                        sub,
                        theme::mono(9.5),
                        sub_col,
                    );

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
                        ui.menu_button("Move to bin", |ui| {
                            for (bid, bname) in &bins {
                                if ui.button(bname).clicked() {
                                    move_to = Some((id, *bid));
                                    ui.close();
                                }
                            }
                        });
                        ui.separator();
                        if ui.button("Remove from project").clicked() {
                            to_remove = Some(id);
                            ui.close();
                        }
                    });
                }
            });

            if let Some(id) = to_open {
                app.project.activate(id);
                app.playhead = app.playhead.min(app.duration());
                app.mark_audio_dirty();
            }
            if let Some((m, b)) = move_to {
                app.snapshot();
                if let Some(item) = app.project.media_mut(m) {
                    item.bin = b;
                }
            }
            if let Some(id) = to_insert {
                app.insert_media(id);
            }
            if let Some(id) = to_remove {
                app.remove_media(id);
            }
        });
}

fn draw_bin(app: &mut App, ui: &mut egui::Ui, bin: u64, depth: usize) {
    let (name, count) = {
        let name = app
            .project
            .bins
            .iter()
            .find(|b| b.id == bin)
            .map(|b| b.name.clone())
            .unwrap_or_default();
        let count = app.project.media.iter().filter(|m| m.bin == bin).count()
            + app.project.timelines.iter().filter(|t| t.bin == bin).count();
        (name, count)
    };
    let on = app.active_bin == bin;
    let resp = ui.allocate_response(Vec2::new(ui.available_width(), 20.0), Sense::click());
    let r = resp.rect;
    if on {
        ui.painter().rect_filled(r, CornerRadius::same(3), theme::ACCENT_DIM);
    } else if resp.hovered() {
        ui.painter().rect_filled(r, CornerRadius::same(3), theme::PANEL_HI);
    }
    let x = r.left() + 6.0 + depth as f32 * 12.0;
    ui.painter().text(
        Pos2::new(x, r.center().y),
        Align2::LEFT_CENTER,
        format!("{name}  ({count})"),
        theme::ui_font(11.0),
        if on { theme::TEXT } else { theme::TEXT_DIM },
    );
    if resp.clicked() {
        app.active_bin = bin;
    }
    resp.context_menu(|ui| {
        if ui.button("New sub-bin").clicked() {
            app.snapshot();
            let id = app.project.add_bin("New bin", Some(bin));
            app.active_bin = id;
            ui.close();
        }
        if ui.button("Rename…").clicked() {
            app.rename_bin = Some(bin);
            ui.close();
        }
    });

    let kids: Vec<u64> = app
        .project
        .bins
        .iter()
        .filter(|b| b.parent == Some(bin))
        .map(|b| b.id)
        .collect();
    for k in kids {
        draw_bin(app, ui, k, depth + 1);
    }
}

pub fn inspector(app: &mut App, ctx: &Context) {
    egui::SidePanel::right("inspector")
        .resizable(true)
        .default_width(272.0)
        .width_range(220.0..=440.0)
        .frame(egui::Frame::NONE.fill(theme::PANEL).inner_margin(egui::Margin::same(10)))
        .show(ctx, |ui| {
            let sel = app.project.selected_ids();
            if sel.is_empty() {
                ui.label(
                    egui::RichText::new("INSPECTOR").font(theme::mono(10.0)).color(theme::TEXT_FAINT),
                );
                ui.add_space(18.0);
                ui.vertical_centered(|ui| {
                    ui.label(
                        egui::RichText::new("Nothing selected")
                            .color(theme::TEXT_DIM)
                            .size(13.0),
                    );
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new("Click a clip on the timeline\nto change how it looks and sounds.")
                            .color(theme::TEXT_FAINT)
                            .size(11.5),
                    );
                });
                return;
            }

            let cid = sel[0];
            let Some((track, clip)) = app.project.clip(cid) else { return };
            let kind = track.kind;
            let fps = app.project.seq().fps;
            let mut c = clip.clone();
            let mut changed = false;
            let mut speed_change: Option<f32> = None;
            let mut crossfade: Option<i64> = None;

            let title = match &c.source {
                ClipSource::Media(m) => {
                    app.project.media(*m).map(|m| m.name.clone()).unwrap_or_default()
                }
                ClipSource::Text(_) => "Title".into(),
                ClipSource::Color(_) => "Colour card".into(),
            };
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(title).strong().size(13.0));
                if sel.len() > 1 {
                    ui.label(
                        egui::RichText::new(format!("+{}", sel.len() - 1))
                            .font(theme::mono(10.0))
                            .color(theme::ACCENT),
                    );
                }
            });
            ui.label(
                egui::RichText::new(format!(
                    "{}  →  {}      {}",
                    timecode(c.start, fps),
                    timecode(c.end(), fps),
                    timecode(c.len, fps)
                ))
                .font(theme::mono(10.0))
                .color(theme::TEXT_FAINT),
            );
            ui.add_space(4.0);
            ui.separator();

            egui::ScrollArea::vertical().show(ui, |ui| {
                if let ClipSource::Text(t) = &mut c.source {
                    theme::section(ui, "TEXT");
                    if ui.add(egui::TextEdit::multiline(&mut t.text).desired_rows(2)).changed() {
                        changed = true;
                    }
                    changed |= theme::row(ui, "Size", |ui| {
                        ui.add(egui::Slider::new(&mut t.size, 0.02..=0.35).show_value(false))
                    })
                    .changed();
                    theme::row(ui, "Align", |ui| {
                        ui.horizontal(|ui| {
                            for (a, l) in [
                                (TextAlign::Left, "Left"),
                                (TextAlign::Center, "Centre"),
                                (TextAlign::Right, "Right"),
                            ] {
                                if ui.selectable_label(t.align == a, l).clicked() {
                                    t.align = a;
                                    changed = true;
                                }
                            }
                        })
                        .response
                    });
                    changed |= theme::row(ui, "Across", |ui| {
                        ui.add(egui::Slider::new(&mut t.x, 0.0..=1.0).show_value(false))
                    })
                    .changed();
                    changed |= theme::row(ui, "Down", |ui| {
                        ui.add(egui::Slider::new(&mut t.y, 0.0..=1.0).show_value(false))
                    })
                    .changed();
                    let mut col = Color32::from_rgba_unmultiplied(
                        t.color[0], t.color[1], t.color[2], t.color[3],
                    );
                    theme::row(ui, "Colour", |ui| {
                        let r = ui.color_edit_button_srgba(&mut col);
                        if r.changed() {
                            t.color = [col.r(), col.g(), col.b(), col.a()];
                            changed = true;
                        }
                        r
                    });
                    changed |= ui.checkbox(&mut t.shadow, "Drop shadow").changed();
                    changed |= ui.checkbox(&mut t.box_bg, "Background box").changed();
                }

                if kind == TrackKind::Video {
                    theme::section(ui, "PICTURE");
                    changed |= theme::row(ui, "Opacity", |ui| {
                        ui.add(egui::Slider::new(&mut c.opacity, 0.0..=1.0))
                    })
                    .changed();
                    changed |= theme::row(ui, "Scale", |ui| {
                        ui.add(egui::Slider::new(&mut c.scale, 0.05..=4.0).logarithmic(true))
                    })
                    .changed();
                    changed |= theme::row(ui, "Across", |ui| {
                        ui.add(egui::Slider::new(&mut c.pos_x, -1.0..=1.0))
                    })
                    .changed();
                    changed |= theme::row(ui, "Down", |ui| {
                        ui.add(egui::Slider::new(&mut c.pos_y, -1.0..=1.0))
                    })
                    .changed();
                    if ui.small_button("Reset picture").clicked() {
                        c.scale = 1.0;
                        c.pos_x = 0.0;
                        c.pos_y = 0.0;
                        c.opacity = 1.0;
                        changed = true;
                    }

                    theme::section(ui, "COLOUR");
                    changed |= theme::row(ui, "Brightness", |ui| {
                        ui.add(egui::Slider::new(&mut c.color.brightness, -0.5..=0.5))
                    })
                    .changed();
                    changed |= theme::row(ui, "Contrast", |ui| {
                        ui.add(egui::Slider::new(&mut c.color.contrast, 0.0..=2.5))
                    })
                    .changed();
                    changed |= theme::row(ui, "Saturation", |ui| {
                        ui.add(egui::Slider::new(&mut c.color.saturation, 0.0..=2.5))
                    })
                    .changed();
                    ui.horizontal(|ui| {
                        for (label, v) in [
                            ("Punchy", ColorAdjust { brightness: 0.02, contrast: 1.15, saturation: 1.18 }),
                            ("Flat", ColorAdjust { brightness: 0.0, contrast: 0.88, saturation: 0.9 }),
                            ("Mono", ColorAdjust { brightness: 0.0, contrast: 1.05, saturation: 0.0 }),
                            ("None", ColorAdjust::default()),
                        ] {
                            if ui.small_button(label).clicked() {
                                c.color = v;
                                changed = true;
                            }
                        }
                    });
                }

                let has_audio = c
                    .media_id()
                    .and_then(|m| app.project.media(m).map(|m| m.has_audio))
                    .unwrap_or(false);
                if has_audio {
                    theme::section(ui, "SOUND");
                    let mut db = if c.volume <= 0.0001 { -60.0 } else { 20.0 * c.volume.log10() };
                    let r = theme::row(ui, "Volume", |ui| {
                        ui.add(egui::Slider::new(&mut db, -60.0..=12.0).suffix(" dB"))
                    });
                    if r.changed() {
                        c.volume = if db <= -59.9 { 0.0 } else { 10f32.powf(db / 20.0) };
                        changed = true;
                    }
                    if c.volume <= 0.0001 {
                        ui.label(
                            egui::RichText::new("Silent — this clip will have no sound on export")
                                .color(theme::WARN)
                                .size(11.0),
                        );
                    }
                }

                theme::section(ui, "SPEED");
                let mut speed = c.speed;
                let sr = theme::row(ui, "Rate", |ui| {
                    ui.add(egui::Slider::new(&mut speed, 0.25..=4.0).logarithmic(true).suffix("×"))
                });
                ui.horizontal(|ui| {
                    for (label, v) in [("½×", 0.5f32), ("1×", 1.0), ("2×", 2.0), ("4×", 4.0)] {
                        if ui.small_button(label).clicked() {
                            speed = v;
                        }
                    }
                });
                if sr.changed() || (speed - c.speed).abs() > 1e-4 {
                    speed_change = Some(speed);
                }

                theme::section(ui, "TRANSITION");
                let mut tr = c.transition_in;
                let max_tr = c.len.min(fps as i64 * 3).max(1);
                if theme::row(ui, "Crossfade", |ui| {
                    ui.add(egui::Slider::new(&mut tr, 0..=max_tr).suffix(" fr"))
                })
                .changed()
                {
                    c.transition_in = tr;
                    changed = true;
                }
                if ui.small_button("½ second dissolve").clicked() {
                    crossfade = Some((fps as i64 / 2).max(1));
                }

                theme::section(ui, "FADES");
                let max_fade = (c.len / 2).max(1);
                let mut fi = c.fade_in;
                let mut fo = c.fade_out;
                if theme::row(ui, "In", |ui| {
                    ui.add(egui::Slider::new(&mut fi, 0..=max_fade).suffix(" fr"))
                })
                .changed()
                {
                    c.fade_in = fi;
                    changed = true;
                }
                if theme::row(ui, "Out", |ui| {
                    ui.add(egui::Slider::new(&mut fo, 0..=max_fade).suffix(" fr"))
                })
                .changed()
                {
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
                    if ui.small_button("None").clicked() {
                        c.fade_in = 0;
                        c.fade_out = 0;
                        changed = true;
                    }
                });
                ui.add_space(12.0);
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
        .frame(egui::Frame::NONE.fill(theme::PANEL).inner_margin(egui::Margin::symmetric(10, 7)))
        .show(ctx, |ui| {
            // Three explicit regions. A right-to-left sub-layout claims all remaining width, so
            // anything centred has to be given its own reserved space rather than added after.
            ui.horizontal(|ui| {
                let total = ui.available_width();
                let side = (total * 0.3).clamp(150.0, 260.0);
                let mid = (total - side * 2.0).max(150.0);
                let h = 30.0;

                ui.allocate_ui_with_layout(
                    Vec2::new(side, h),
                    Layout::left_to_right(Align::Center),
                    |ui| {
                        ui.label(
                            egui::RichText::new(timecode(app.playhead, app.fps()))
                                .font(theme::mono(18.0))
                                .color(theme::TEXT),
                        );
                        ui.label(
                            egui::RichText::new(timecode(app.duration(), app.fps()))
                                .font(theme::mono(10.5))
                                .color(theme::TEXT_FAINT),
                        );
                    },
                );

                ui.allocate_ui_with_layout(
                    Vec2::new(mid, h),
                    Layout::top_down(Align::Center),
                    |ui| {
                        ui.horizontal(|ui| {
                            if ui.button("⏮").on_hover_text("Go to start   Home").clicked() {
                                app.set_playhead(0);
                            }
                            if ui.button("◀").on_hover_text("Back one frame   ←").clicked() {
                                let p = app.playhead - 1;
                                app.set_playhead(p);
                            }
                            let label = if app.playing { "⏸" } else { "▶" };
                            if ui
                                .add(
                                    egui::Button::new(egui::RichText::new(label).size(16.0))
                                        .min_size(Vec2::new(56.0, 28.0)),
                                )
                                .on_hover_text("Play or pause   Space")
                                .clicked()
                            {
                                app.toggle_play();
                            }
                            if ui.button("▶").on_hover_text("Forward one frame   →").clicked() {
                                let p = app.playhead + 1;
                                app.set_playhead(p);
                            }
                            if ui.button("⏭").on_hover_text("Go to end   End").clicked() {
                                let d = app.duration();
                                app.set_playhead(d);
                            }
                        });
                    },
                );

                ui.allocate_ui_with_layout(
                    Vec2::new(side, h),
                    Layout::right_to_left(Align::Center),
                    |ui| {
                        meters(app, ui);
                    },
                );
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
    project_manager(app, ctx);
    new_project_dialog(app, ctx);
    new_timeline_dialog(app, ctx);
    rename_bin_dialog(app, ctx);
    add_track_dialog(app, ctx);
    render_queue_window(app, ctx);
    output_module_dialog(app, ctx);
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

/// The start screen. Previously a project simply appeared, already called "Untitled" and already
/// 1920x1080 at 30fps, with no indication that any of that was a choice.
/// The window Kite opens with: your projects, and a way to start another.
fn project_manager(app: &mut App, ctx: &Context) {
    if !app.show_project_manager {
        return;
    }
    egui::Window::new("Projects")
        .collapsible(false)
        .resizable(true)
        .default_width(560.0)
        .default_height(400.0)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui
                    .add(egui::Button::new("New project").min_size(Vec2::new(120.0, 26.0)))
                    .clicked()
                {
                    app.show_new_project = true;
                }
                if ui.button("Open from disk…").clicked() {
                    app.open();
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(
                            crate::app::projects_dir().display().to_string(),
                        )
                        .font(theme::mono(9.5))
                        .color(theme::TEXT_FAINT),
                    );
                });
            });
            ui.add_space(8.0);
            ui.separator();

            let projects = App::list_projects();
            if projects.is_empty() {
                ui.add_space(30.0);
                ui.vertical_centered(|ui| {
                    ui.label(
                        egui::RichText::new("No projects yet").color(theme::TEXT_DIM).size(14.0),
                    );
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new("Start one and it will appear here next time.")
                            .color(theme::TEXT_FAINT)
                            .size(11.5),
                    );
                });
                return;
            }

            let mut open_path = None;
            let mut delete_path = None;
            egui::ScrollArea::vertical().show(ui, |ui| {
                for (path, name, modified) in projects {
                    let resp =
                        ui.allocate_response(Vec2::new(ui.available_width(), 42.0), Sense::click());
                    let r = resp.rect;
                    if resp.hovered() {
                        ui.painter().rect_filled(r, CornerRadius::same(4), theme::PANEL_HI);
                    }
                    let sw = Rect::from_min_size(
                        Pos2::new(r.left() + 8.0, r.center().y - 11.0),
                        Vec2::new(38.0, 22.0),
                    );
                    ui.painter().rect_filled(sw, CornerRadius::same(3), theme::RAISED);
                    ui.painter().text(
                        Pos2::new(r.left() + 56.0, r.top() + 6.0),
                        Align2::LEFT_TOP,
                        &name,
                        theme::ui_font(13.0),
                        theme::TEXT,
                    );
                    let ago = modified
                        .elapsed()
                        .map(|d| {
                            let s = d.as_secs();
                            if s < 3600 {
                                format!("{} min ago", (s / 60).max(1))
                            } else if s < 86400 {
                                format!("{} hours ago", s / 3600)
                            } else {
                                format!("{} days ago", s / 86400)
                            }
                        })
                        .unwrap_or_else(|_| "just now".into());
                    ui.painter().text(
                        Pos2::new(r.left() + 56.0, r.bottom() - 6.0),
                        Align2::LEFT_BOTTOM,
                        ago,
                        theme::mono(9.5),
                        theme::TEXT_FAINT,
                    );
                    if resp.clicked() || resp.double_clicked() {
                        open_path = Some(path.clone());
                    }
                    resp.context_menu(|ui| {
                        if ui.button("Open").clicked() {
                            open_path = Some(path.clone());
                            ui.close();
                        }
                        if ui.button(egui::RichText::new("Delete").color(theme::BAD)).clicked() {
                            delete_path = Some(path.clone());
                            ui.close();
                        }
                    });
                }
            });
            if let Some(p) = open_path {
                app.open_path(p);
                app.show_project_manager = false;
            }
            if let Some(p) = delete_path {
                std::fs::remove_file(p).ok();
            }
        });
}

fn new_project_dialog(app: &mut App, ctx: &Context) {
    if !app.show_new_project {
        return;
    }
    egui::Window::new("New project")
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .show(ctx, |ui| {
            ui.set_min_width(420.0);
            ui.add_space(2.0);
            theme::row(ui, "Name", |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut app.new_proj.name)
                        .desired_width(f32::INFINITY)
                        .hint_text("My video"),
                )
            });

            ui.add_space(10.0);
            ui.checkbox(
                &mut app.new_proj.from_first_clip,
                "Match the first clip I add",
            )
            .on_hover_text("The sequence takes the size and frame rate of your footage");
            ui.label(
                egui::RichText::new(
                    "Recommended — you almost never want to work at a different size from your footage.",
                )
                .color(theme::TEXT_FAINT)
                .size(11.0),
            );

            ui.add_space(8.0);
            ui.add_enabled_ui(!app.new_proj.from_first_clip, |ui| {
                theme::row(ui, "Size", |ui| {
                    egui::ComboBox::from_id_salt("npres")
                        .selected_text(format!("{}×{}", app.new_proj.width, app.new_proj.height))
                        .show_ui(ui, |ui| {
                            for (w, h, name) in RESOLUTIONS {
                                if ui
                                    .selectable_label(
                                        app.new_proj.width == w && app.new_proj.height == h,
                                        name,
                                    )
                                    .clicked()
                                {
                                    app.new_proj.width = w;
                                    app.new_proj.height = h;
                                }
                            }
                        })
                        .response
                });
                theme::row(ui, "Frame rate", |ui| {
                    egui::ComboBox::from_id_salt("npfps")
                        .selected_text(format!("{} fps", app.new_proj.fps))
                        .show_ui(ui, |ui| {
                            for f in [24u32, 25, 30, 50, 60] {
                                if ui
                                    .selectable_label(app.new_proj.fps == f, format!("{f} fps"))
                                    .clicked()
                                {
                                    app.new_proj.fps = f;
                                }
                            }
                        })
                        .response
                });
            });

            ui.add_space(14.0);
            ui.horizontal(|ui| {
                if ui
                    .add(egui::Button::new("Start editing").min_size(Vec2::new(130.0, 28.0)))
                    .clicked()
                {
                    app.create_project();
                }
                if ui.button("Cancel").clicked() {
                    app.show_new_project = false;
                }
            });
        });
}

pub const RESOLUTIONS: [(u32, u32, &str); 6] = [
    (1920, 1080, "1920×1080   HD"),
    (2560, 1440, "2560×1440   QHD"),
    (3840, 2160, "3840×2160   4K"),
    (1080, 1920, "1080×1920   Vertical"),
    (1080, 1080, "1080×1080   Square"),
    (1280, 720, "1280×720    720p"),
];

fn add_track_dialog(app: &mut App, ctx: &Context) {
    if !app.show_add_track {
        return;
    }
    egui::Window::new("Add a track")
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .show(ctx, |ui| {
            ui.set_min_width(260.0);
            ui.label(
                egui::RichText::new("Video tracks stack upward — anything on a higher track covers what is below it.")
                    .color(theme::TEXT_DIM)
                    .size(11.5),
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.add(egui::Button::new("Video track").min_size(Vec2::new(110.0, 26.0))).clicked() {
                    app.add_track(TrackKind::Video);
                    app.show_add_track = false;
                }
                if ui.add(egui::Button::new("Audio track").min_size(Vec2::new(110.0, 26.0))).clicked() {
                    app.add_track(TrackKind::Audio);
                    app.show_add_track = false;
                }
            });
            ui.add_space(6.0);
            if ui.small_button("Cancel").clicked() {
                app.show_add_track = false;
            }
        });
}

/// The render queue, modelled on After Effects: rows you add compositions to, each with its own
/// output settings and destination, rendered in order when you press Render.
fn render_queue_window(app: &mut App, ctx: &Context) {
    if !app.show_render_queue {
        return;
    }
    let mut open = true;
    let rendering = app.export_job.is_some();
    egui::Window::new("Render Queue")
        .open(&mut open)
        .collapsible(false)
        .default_width(720.0)
        .default_height(340.0)
        .resizable(true)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui
                    .button("Add this timeline")
                    .on_hover_text("Queue the timeline you are editing   Ctrl+M")
                    .clicked()
                {
                    app.add_to_render_queue();
                }
                ui.separator();
                let any = app
                    .project
                    .render_queue
                    .iter()
                    .any(|r| r.status != RenderStatus::Off);
                if ui
                    .add_enabled(any && !rendering, egui::Button::new("Render"))
                    .on_hover_text("Render every row that is not switched off")
                    .clicked()
                {
                    app.render_all();
                }
                if ui.add_enabled(rendering, egui::Button::new("Stop")).clicked() {
                    app.stop_rendering();
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("Clear finished").clicked() {
                        app.project
                            .render_queue
                            .retain(|r| r.status != RenderStatus::Done);
                    }
                });
            });
            ui.add_space(6.0);

            if app.project.render_queue.is_empty() {
                ui.add_space(24.0);
                ui.vertical_centered(|ui| {
                    ui.label(egui::RichText::new("Nothing queued").color(theme::TEXT_DIM).size(13.0));
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new("Add the timeline you are working on, set where it should go, then press Render.")
                            .color(theme::TEXT_FAINT)
                            .size(11.5),
                    );
                });
                return;
            }

            let mut remove: Option<u64> = None;
            let mut toggle: Option<u64> = None;
            let mut choose_path: Option<u64> = None;
            let mut edit: Option<u64> = None;

            egui::ScrollArea::vertical().show(ui, |ui| {
                egui::Grid::new("rq")
                    .num_columns(5)
                    .striped(true)
                    .spacing(Vec2::new(10.0, 6.0))
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new("").font(theme::mono(9.5)));
                        for h in ["TIMELINE", "STATUS", "OUTPUT MODULE", "OUTPUT TO"] {
                            ui.label(
                                egui::RichText::new(h)
                                    .font(theme::mono(9.5))
                                    .color(theme::TEXT_FAINT),
                            );
                        }
                        ui.end_row();

                        let rows: Vec<_> = app
                            .project
                            .render_queue
                            .iter()
                            .map(|r| {
                                (
                                    r.id,
                                    app.project
                                        .timeline_by_id(r.timeline)
                                        .map(|t| t.name.clone())
                                        .unwrap_or_else(|| "missing".into()),
                                    r.status,
                                    format!(
                                        "{}×{} · {} fps · {} · {}",
                                        r.width,
                                        r.height,
                                        r.fps,
                                        match r.quality {
                                            1 => "Balanced",
                                            2 => "Small",
                                            _ => "High",
                                        },
                                        if r.include_audio { "audio" } else { "no audio" }
                                    ),
                                    r.output.clone(),
                                    r.note.clone(),
                                )
                            })
                            .collect();

                        for (id, tlname, status, module, out, note) in rows {
                            let on = status != RenderStatus::Off;
                            let mut checked = on;
                            if ui.checkbox(&mut checked, "").changed() {
                                toggle = Some(id);
                            }
                            ui.label(egui::RichText::new(tlname).size(12.0));

                            let (label, col) = match status {
                                RenderStatus::Queued => ("Queued".to_string(), theme::TEXT_DIM),
                                RenderStatus::Rendering => (
                                    format!("Rendering {:.0}%", app.export_pct),
                                    theme::ACCENT,
                                ),
                                RenderStatus::Done => ("Done".to_string(), theme::GOOD),
                                RenderStatus::Failed => ("Failed".to_string(), theme::BAD),
                                RenderStatus::Off => ("Off".to_string(), theme::TEXT_FAINT),
                            };
                            ui.label(egui::RichText::new(label).color(col).size(12.0))
                                .on_hover_text(if note.is_empty() { "—".into() } else { note });

                            if ui
                                .add(egui::Button::new(
                                    egui::RichText::new(module).font(theme::mono(10.0)),
                                ))
                                .on_hover_text("Change the format, size and audio for this row")
                                .clicked()
                            {
                                edit = Some(id);
                            }

                            ui.horizontal(|ui| {
                                if ui
                                    .add(egui::Button::new(
                                        egui::RichText::new(elide(
                                            &out.display().to_string(),
                                            30,
                                        ))
                                        .font(theme::mono(10.0)),
                                    ))
                                    .on_hover_text(out.display().to_string())
                                    .clicked()
                                {
                                    choose_path = Some(id);
                                }
                                if ui.small_button("x").on_hover_text("Remove this row").clicked() {
                                    remove = Some(id);
                                }
                            });
                            ui.end_row();
                        }
                    });
            });

            if app.export_job.is_some() {
                ui.add_space(6.0);
                ui.add(egui::ProgressBar::new(app.export_pct / 100.0).show_percentage());
                ui.label(
                    egui::RichText::new(&app.export_note)
                        .font(theme::mono(10.0))
                        .color(theme::TEXT_FAINT),
                );
                ctx.request_repaint_after(std::time::Duration::from_millis(200));
            }

            if let Some(id) = toggle {
                if let Some(r) = app.project.render_queue.iter_mut().find(|r| r.id == id) {
                    r.status = if r.status == RenderStatus::Off {
                        RenderStatus::Queued
                    } else {
                        RenderStatus::Off
                    };
                }
            }
            if let Some(id) = remove {
                app.project.render_queue.retain(|r| r.id != id);
            }
            if let Some(id) = edit {
                app.editing_output = Some(id);
            }
            if let Some(id) = choose_path {
                let current = app
                    .queue_item(id)
                    .map(|r| r.output.clone())
                    .unwrap_or_default();
                let name = current
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "video.mp4".into());
                if let Some(p) = rfd::FileDialog::new()
                    .set_title("Output to")
                    .add_filter("MP4 video", &["mp4"])
                    .set_file_name(&name)
                    .save_file()
                {
                    if let Some(r) = app.project.render_queue.iter_mut().find(|r| r.id == id) {
                        r.output = p;
                    }
                }
            }
        });
    if !open {
        app.show_render_queue = false;
    }
}

/// The per-row settings, in the shape of After Effects' Output Module.
fn output_module_dialog(app: &mut App, ctx: &Context) {
    let Some(id) = app.editing_output else { return };
    let Some(item) = app.queue_item(id).cloned() else {
        app.editing_output = None;
        return;
    };
    let tl_name = app
        .project
        .timeline_by_id(item.timeline)
        .map(|t| t.name.clone())
        .unwrap_or_default();
    let (audio_clips, silent_clips, muted_has_audio) = app
        .project
        .timeline_by_id(item.timeline)
        .map(|t| crate::export::audio_summary(&app.project, t))
        .unwrap_or((0, 0, false));

    let mut open = true;
    let mut next = item.clone();
    egui::Window::new(format!("Output Module — {tl_name}"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .show(ctx, |ui| {
            ui.set_min_width(400.0);
            theme::section(ui, "VIDEO");
            theme::row(ui, "Size", |ui| {
                egui::ComboBox::from_id_salt("omres")
                    .selected_text(format!("{}×{}", next.width, next.height))
                    .show_ui(ui, |ui| {
                        for (w, h, name) in RESOLUTIONS {
                            if ui
                                .selectable_label(next.width == w && next.height == h, name)
                                .clicked()
                            {
                                next.width = w;
                                next.height = h;
                            }
                        }
                    })
                    .response
            });
            theme::row(ui, "Frame rate", |ui| {
                egui::ComboBox::from_id_salt("omfps")
                    .selected_text(format!("{} fps", next.fps))
                    .show_ui(ui, |ui| {
                        for f in [24u32, 25, 30, 50, 60] {
                            if ui.selectable_label(next.fps == f, format!("{f} fps")).clicked() {
                                next.fps = f;
                            }
                        }
                    })
                    .response
            });
            theme::row(ui, "Quality", |ui| {
                egui::ComboBox::from_id_salt("omq")
                    .selected_text(match next.quality {
                        1 => "Balanced",
                        2 => "Small file",
                        _ => "High — best for YouTube",
                    })
                    .show_ui(ui, |ui| {
                        for (i, label) in
                            [(0u8, "High — best for YouTube"), (1, "Balanced"), (2, "Small file")]
                        {
                            if ui.selectable_label(next.quality == i, label).clicked() {
                                next.quality = i;
                            }
                        }
                    })
                    .response
            });
            let encoders = app.encoders.clone();
            theme::row(ui, "Encoder", |ui| {
                egui::ComboBox::from_id_salt("omenc")
                    .selected_text(
                        encoders
                            .get(next.encoder as usize)
                            .map(|e| e.label())
                            .unwrap_or("x264"),
                    )
                    .show_ui(ui, |ui| {
                        for (i, e) in encoders.iter().enumerate() {
                            if ui.selectable_label(next.encoder as usize == i, e.label()).clicked() {
                                next.encoder = i as u8;
                            }
                        }
                    })
                    .response
            });

            theme::section(ui, "AUDIO");
            ui.checkbox(&mut next.include_audio, "Include audio");
            let (msg, col) = if !next.include_audio {
                ("Audio is switched off — the file will be silent.".to_string(), theme::WARN)
            } else if audio_clips == 0 && muted_has_audio {
                ("No sound: every clip with audio is on a muted track.".to_string(), theme::BAD)
            } else if audio_clips == 0 {
                ("No sound: nothing on this timeline has an audio track.".to_string(), theme::WARN)
            } else if silent_clips == audio_clips {
                ("No sound: every audio clip is at zero volume.".to_string(), theme::BAD)
            } else {
                (format!("{audio_clips} clip(s) with sound"), theme::GOOD)
            };
            ui.label(egui::RichText::new(msg).color(col).size(11.5));

            ui.add_space(10.0);
            if ui.add(egui::Button::new("Done").min_size(Vec2::new(90.0, 26.0))).clicked() {
                app.editing_output = None;
            }
        });

    if let Some(r) = app.project.render_queue.iter_mut().find(|r| r.id == id) {
        if *r != next {
            *r = next;
        }
    }
    if !open {
        app.editing_output = None;
    }
}

fn new_timeline_dialog(app: &mut App, ctx: &Context) {
    if !app.show_new_timeline {
        return;
    }
    let mut open = true;
    egui::Window::new("New timeline")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .show(ctx, |ui| {
            ui.set_min_width(360.0);
            theme::row(ui, "Name", |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut app.new_timeline_name)
                        .desired_width(f32::INFINITY),
                )
            });
            ui.add_space(6.0);
            let p = app.project.settings;
            ui.checkbox(&mut app.new_timeline_custom, "Use a different format for this timeline");
            if !app.new_timeline_custom {
                ui.label(
                    egui::RichText::new(format!(
                        "Project format: {}×{} at {} fps",
                        p.width, p.height, p.fps
                    ))
                    .color(theme::TEXT_FAINT)
                    .size(11.0),
                );
            } else {
                let mut w = app.new_proj.width;
                let mut h = app.new_proj.height;
                theme::row(ui, "Size", |ui| {
                    egui::ComboBox::from_id_salt("ntres")
                        .selected_text(format!("{w}×{h}"))
                        .show_ui(ui, |ui| {
                            for (rw, rh, name) in RESOLUTIONS {
                                if ui.selectable_label(w == rw && h == rh, name).clicked() {
                                    w = rw;
                                    h = rh;
                                }
                            }
                        })
                        .response
                });
                theme::row(ui, "Frame rate", |ui| {
                    egui::ComboBox::from_id_salt("ntfps")
                        .selected_text(format!("{} fps", app.new_proj.fps))
                        .show_ui(ui, |ui| {
                            for f in [24u32, 25, 30, 50, 60] {
                                if ui
                                    .selectable_label(app.new_proj.fps == f, format!("{f} fps"))
                                    .clicked()
                                {
                                    app.new_proj.fps = f;
                                }
                            }
                        })
                        .response
                });
                app.new_proj.width = w;
                app.new_proj.height = h;
            }
            ui.add_space(10.0);
            if ui.add(egui::Button::new("Create").min_size(Vec2::new(100.0, 26.0))).clicked() {
                app.snapshot();
                let fmt = if app.new_timeline_custom {
                    Some(crate::project::VideoSettings {
                        width: app.new_proj.width,
                        height: app.new_proj.height,
                        fps: app.new_proj.fps,
                    })
                } else {
                    None
                };
                let name = app.new_timeline_name.clone();
                app.project.add_timeline(&name, fmt);
                app.playhead = 0;
                app.mark_audio_dirty();
                app.show_new_timeline = false;
            }
        });
    if !open {
        app.show_new_timeline = false;
    }
}

fn rename_bin_dialog(app: &mut App, ctx: &Context) {
    let Some(bin) = app.rename_bin else { return };
    let mut open = true;
    let mut name = app
        .project
        .bins
        .iter()
        .find(|b| b.id == bin)
        .map(|b| b.name.clone())
        .unwrap_or_default();
    egui::Window::new("Rename bin")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .show(ctx, |ui| {
            ui.set_min_width(280.0);
            if ui.add(egui::TextEdit::singleline(&mut name).desired_width(f32::INFINITY)).changed() {
                if let Some(b) = app.project.bins.iter_mut().find(|b| b.id == bin) {
                    b.name = name.clone();
                }
                app.dirty = true;
            }
            if ui.button("Done").clicked() {
                app.rename_bin = None;
            }
        });
    if !open {
        app.rename_bin = None;
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
