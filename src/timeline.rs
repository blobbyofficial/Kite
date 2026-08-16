//! The timeline panel: ruler, track headers, clips, and all direct manipulation.
//!
//! Everything is drawn with the painter rather than widgets, and only the visible frame range is
//! considered, so cost is a function of what is on screen rather than how long the sequence is.

use crate::app::{clip_colors, App, DragKind, DragState};
use crate::import::PEAK_BUCKET;
use crate::project::{timecode, ClipId, ClipSource, TrackId, TrackKind};
use crate::theme;
use egui::{Align2, Color32, Context, CornerRadius, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};

const HEADER_W: f32 = 132.0;
const RULER_H: f32 = 26.0;
const EDGE_GRAB: f32 = 7.0;

pub fn panel(app: &mut App, ctx: &Context) {
    egui::TopBottomPanel::bottom("timeline")
        .resizable(true)
        .default_height(320.0)
        .height_range(180.0..=900.0)
        .frame(egui::Frame::NONE.fill(theme::PANEL))
        .show(ctx, |ui| {
            toolbar(app, ui);
            ui.separator();
            let rect = ui.available_rect_before_wrap();
            if rect.height() > 30.0 {
                body(app, ui, rect);
                // The panel remembers its content height between frames, and everything below is
                // painted rather than laid out. Without claiming the space the panel would shrink
                // to the height of the toolbar and stay there.
                ui.allocate_rect(rect, Sense::hover());
            }
        });
}

/// Editing actions on the left, view controls on the right.
///
/// The first version put everything in one flat row — cutting tools, view toggles and "add track"
/// side by side — which made it impossible to find anything by category.
fn toolbar(app: &mut App, ui: &mut egui::Ui) {
    let has_sel = app.project.tracks().iter().any(|t| t.clips.iter().any(|c| c.selected));
    // Which sequence you are editing, and a way to switch — a project can hold several.
    let cur = app.project.tl().name.clone();
    let items: Vec<(u64, String)> = app
        .project
        .timelines
        .iter()
        .map(|t| (t.id, t.name.clone()))
        .collect();
    let active = app.project.tl().id;
    let mut switch = None;
    ui.horizontal(|ui| {
        ui.add_space(2.0);
        egui::ComboBox::from_id_salt("tlpick")
            .selected_text(egui::RichText::new(&cur).size(12.0))
            .width(160.0)
            .show_ui(ui, |ui| {
                for (id, name) in &items {
                    if ui.selectable_label(*id == active, name).clicked() {
                        switch = Some(*id);
                    }
                }
                ui.separator();
                if ui.button("New timeline…").clicked() {
                    app.show_new_timeline = true;
                }
            });
        ui.label(
            egui::RichText::new(format!(
                "{}×{} · {} fps",
                app.project.seq().width,
                app.project.seq().height,
                app.project.seq().fps
            ))
            .font(theme::mono(10.0))
            .color(theme::TEXT_FAINT),
        );
    });
    if let Some(id) = switch {
        app.project.activate(id);
        app.playhead = app.playhead.min(app.duration());
        app.mark_audio_dirty();
    }
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        ui.add_space(2.0);
        if ui.button("Split").on_hover_text("Cut every clip at the playhead   S").clicked() {
            app.split_at_playhead();
        }
        if ui
            .add_enabled(has_sel, egui::Button::new("Delete"))
            .on_hover_text("Remove the selection, leaving a gap   Del")
            .clicked()
        {
            app.delete_selected(false);
        }
        if ui
            .add_enabled(has_sel, egui::Button::new("Ripple"))
            .on_hover_text("Remove the selection and close the gap   Shift+Del")
            .clicked()
        {
            app.delete_selected(true);
        }
        if ui
            .add_enabled(has_sel, egui::Button::new("Crossfade"))
            .on_hover_text("Dissolve into the selection from the clip before it   Ctrl+T")
            .clicked()
        {
            let half = (app.fps() as i64 / 2).max(1);
            app.crossfade_selected(half);
        }

        ui.add_space(6.0);
        ui.separator();
        ui.add_space(6.0);
        if ui.button("Title").on_hover_text("Add a title at the playhead").clicked() {
            app.add_text_clip();
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(2.0);
            if ui.button("Fit").on_hover_text("Zoom so the whole sequence fits").clicked() {
                fit_to_window(app);
            }
            if ui.button("+").on_hover_text("Zoom in   +").clicked() {
                app.px_per_frame = (app.px_per_frame * 1.35).min(80.0);
            }
            if ui.button("−").on_hover_text("Zoom out   −").clicked() {
                app.px_per_frame = (app.px_per_frame / 1.35).max(0.02);
            }
            ui.add_space(6.0);
            ui.separator();
            ui.add_space(6.0);
            ui.checkbox(&mut app.follow, "Follow")
                .on_hover_text("Keep the playhead in view while playing");
            ui.checkbox(&mut app.snap, "Snap")
                .on_hover_text("Snap edits to clip edges and the playhead   M");
        });
    });
}

fn fit_to_window(app: &mut App) {
    let d = app.duration().max(1) as f32;
    // Uses the measured lane width; asking the toolbar for its remaining width gave a fraction of
    // the timeline and zoomed far too far out.
    app.px_per_frame = ((app.lane_width - 16.0) / d).clamp(0.02, 80.0);
    app.scroll_x = 0.0;
}

/// Frame interval between thumbnails on a clip. Quantised to a power of two so the thumbnails
/// stay anchored to the clip while zooming instead of shimmering.
fn thumb_step(px_per_frame: f32, thumb_w: f32) -> i64 {
    let raw = (thumb_w / px_per_frame).max(1.0);
    let p = raw.log2().round().clamp(0.0, 20.0);
    (2f32.powf(p) as i64).max(1)
}

/// The half-open range of thumbnail indices on a clip that can appear in `[first, last]`.
fn visible_thumb_range(
    start: i64,
    len: i64,
    step: i64,
    span_frames: f32,
    first: i64,
    last: i64,
) -> (i64, i64) {
    let count = (len + step - 1) / step;
    // A thumbnail drawn at index k covers roughly [k*step, k*step + span_frames).
    let lo = ((first - start) as f32 - span_frames) / step as f32;
    let hi = (last - start) as f32 / step as f32;
    let k0 = (lo.floor() as i64).clamp(0, count);
    let k1 = (hi.ceil() as i64 + 1).clamp(k0, count);
    (k0, k1)
}

/// Uploads the thumbnails visible right now. Only frames already decoded are used, so this never
/// blocks the UI thread; anything missing is queued and appears a frame or two later.
fn ensure_thumbs(app: &mut App, ctx: &Context, lane: Rect, tracks_rect: Rect) {
    let mut wanted: Vec<(crate::project::MediaId, u32)> = Vec::new();
    let mut y = tracks_rect.top();
    let first = frame_at(app, lane, lane.left()) - 1;
    let last = frame_at(app, lane, lane.right()) + 1;

    for t in app.project.tracks() {
        let h = t.height;
        if y > tracks_rect.bottom() {
            break;
        }
        if t.kind != TrackKind::Video || h < 34.0 {
            y += h;
            continue;
        }
        let tw = (h - 6.0) * app.project.seq().aspect();
        if tw >= 8.0 {
            let step = thumb_step(app.px_per_frame, tw);
            for c in &t.clips {
                if c.end() < first || c.start > last {
                    continue;
                }
                let Some(mid) = c.media_id() else { continue };
                if !app.project.media(mid).map(|m| m.has_video).unwrap_or(false) {
                    continue;
                }
                let span = tw / app.px_per_frame;
                let (k0, k1) = visible_thumb_range(c.start, c.len, step, span, first, last);
                for k in k0..k1 {
                    if wanted.len() >= 64 {
                        break;
                    }
                    wanted.push((mid, (c.src_in + k * step).max(0) as u32));
                }
            }
        }
        y += h;
    }

    app.upload_thumbs(ctx, &wanted);
}

fn body(app: &mut App, ui: &mut egui::Ui, rect: Rect) {
    let head_rect = Rect::from_min_max(rect.min, Pos2::new(rect.left() + HEADER_W, rect.bottom()));
    let lane_rect = Rect::from_min_max(Pos2::new(rect.left() + HEADER_W, rect.top()), rect.max);
    let ruler_rect = Rect::from_min_max(
        lane_rect.min,
        Pos2::new(lane_rect.right(), lane_rect.top() + RULER_H),
    );
    let tracks_rect = Rect::from_min_max(
        Pos2::new(lane_rect.left(), lane_rect.top() + RULER_H),
        lane_rect.max,
    );

    // ---- input: zoom and scroll over the lane area -------------------------
    let lane_resp = ui.interact(lane_rect, ui.id().with("lane"), Sense::click_and_drag());
    let pointer = ui.ctx().pointer_latest_pos();
    ui.ctx().input(|i| {
        if i.raw_scroll_delta.y != 0.0 || i.raw_scroll_delta.x != 0.0 {
            let hovering = pointer.map(|p| lane_rect.contains(p)).unwrap_or(false);
            if hovering {
                if i.modifiers.command || i.modifiers.ctrl {
                    // Zoom about the pointer so the frame under the cursor stays put.
                    let anchor = pointer.unwrap_or(lane_rect.center());
                    let f_at = frame_at(app, lane_rect, anchor.x);
                    let factor = (i.raw_scroll_delta.y * 0.004).exp();
                    app.px_per_frame = (app.px_per_frame * factor).clamp(0.02, 80.0);
                    let new_x = x_of(app, lane_rect, f_at);
                    app.scroll_x += new_x - anchor.x;
                } else if i.modifiers.shift {
                    app.scroll_y -= i.raw_scroll_delta.y;
                } else {
                    // Trackpads report horizontal directly; a wheel only has a vertical axis, and
                    // in a timeline that should move you along the sequence.
                    let dx = if i.raw_scroll_delta.x.abs() > 0.01 {
                        i.raw_scroll_delta.x
                    } else {
                        i.raw_scroll_delta.y
                    };
                    app.scroll_x -= dx;
                }
            }
        }
    });

    let max_scroll = (app.duration() as f32 * app.px_per_frame - lane_rect.width() + 200.0).max(0.0);
    app.scroll_x = app.scroll_x.clamp(0.0, max_scroll);

    let content_h: f32 = app.project.tracks().iter().map(|t| t.height).sum();
    let max_scroll_y = (content_h - tracks_rect.height()).max(0.0);
    app.scroll_y = app.scroll_y.clamp(0.0, max_scroll_y);

    if app.follow && app.playing {
        let x = x_of(app, lane_rect, app.playhead);
        if x > lane_rect.right() - 80.0 || x < lane_rect.left() {
            app.scroll_x += x - (lane_rect.left() + lane_rect.width() * 0.35);
            app.scroll_x = app.scroll_x.clamp(0.0, max_scroll);
        }
    }

    ensure_thumbs(app, ui.ctx(), lane_rect, tracks_rect);

    app.lane_width = lane_rect.width();

    let painter = ui.painter_at(rect);
    painter.rect_filled(head_rect, CornerRadius::ZERO, theme::PANEL_HI);
    painter.rect_filled(lane_rect, CornerRadius::ZERO, theme::BG);

    draw_ruler(app, ui, &painter, ruler_rect);

    // ---- tracks ------------------------------------------------------------
    let mut y = tracks_rect.top() - app.scroll_y;
    let track_ids: Vec<(TrackId, f32)> = app
        .project
        .tracks()
        .iter()
        .map(|t| (t.id, t.height))
        .collect();

    let mut hovered_clip: Option<(ClipId, TrackId, DragKind)> = None;

    for (tid, th) in &track_ids {
        let t_rect = Rect::from_min_size(
            Pos2::new(tracks_rect.left(), y),
            Vec2::new(tracks_rect.width(), *th),
        );
        let h_rect = Rect::from_min_size(Pos2::new(head_rect.left(), y), Vec2::new(HEADER_W, *th));
        if y > tracks_rect.bottom() {
            break;
        }
        if y + th >= tracks_rect.top() {
            draw_track_header(app, ui, *tid, h_rect);
            let hit = draw_track(app, ui, &painter, *tid, t_rect);
            if hovered_clip.is_none() {
                hovered_clip = hit;
            }
        }
        y += th;
        painter.line_segment(
            [Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)],
            Stroke::new(1.0, theme::LINE),
        );
    }

    // "Add track" belongs with the tracks, not in the clip-editing toolbar.
    let add_y = y + 4.0;
    if add_y + 22.0 < rect.bottom() {
        let br = Rect::from_min_size(Pos2::new(head_rect.left() + 10.0, add_y), Vec2::new(108.0, 20.0));
        let resp = ui.interact(br, ui.id().with("addtrack"), Sense::click());
        painter.rect_filled(
            br,
            CornerRadius::same(3),
            if resp.hovered() { theme::RAISED } else { theme::PANEL },
        );
        painter.rect_stroke(br, CornerRadius::same(3), Stroke::new(1.0, theme::LINE), StrokeKind::Inside);
        painter.text(
            br.center(),
            Align2::CENTER_CENTER,
            "+ Add track",
            theme::ui_font(11.0),
            theme::TEXT_DIM,
        );
        if resp.clicked() {
            app.show_add_track = true;
        }
    }

    if max_scroll_y > 0.0 {
        let frac = (tracks_rect.height() / content_h).clamp(0.05, 1.0);
        let bar_h = tracks_rect.height() * frac;
        let t = app.scroll_y / max_scroll_y;
        let top = tracks_rect.top() + (tracks_rect.height() - bar_h) * t;
        painter.rect_filled(
            Rect::from_min_size(
                Pos2::new(rect.right() - 5.0, top),
                Vec2::new(3.0, bar_h),
            ),
            CornerRadius::same(2),
            theme::LINE_2_OR_DIM,
        );
    }

    handle_interaction(app, ui, lane_rect, tracks_rect, &lane_resp, hovered_clip);

    // ---- playhead ----------------------------------------------------------
    let px = x_of(app, lane_rect, app.playhead);
    if px >= lane_rect.left() - 1.0 && px <= lane_rect.right() + 1.0 {
        painter.line_segment(
            [Pos2::new(px, lane_rect.top()), Pos2::new(px, lane_rect.bottom())],
            Stroke::new(1.5, theme::PLAYHEAD),
        );
        painter.add(egui::Shape::convex_polygon(
            vec![
                Pos2::new(px - 6.0, lane_rect.top()),
                Pos2::new(px + 6.0, lane_rect.top()),
                Pos2::new(px, lane_rect.top() + 9.0),
            ],
            theme::PLAYHEAD,
            Stroke::NONE,
        ));
    }
}

fn x_of(app: &App, lane: Rect, frame: i64) -> f32 {
    lane.left() + frame as f32 * app.px_per_frame - app.scroll_x
}
fn frame_at(app: &App, lane: Rect, x: f32) -> i64 {
    (((x - lane.left()) + app.scroll_x) / app.px_per_frame).round() as i64
}

fn draw_ruler(app: &mut App, ui: &mut egui::Ui, painter: &egui::Painter, r: Rect) {
    painter.rect_filled(r, CornerRadius::ZERO, theme::PANEL);
    let fps = app.fps() as f32;

    // Choose a tick spacing that keeps labels comfortably apart at any zoom.
    let target_px = 90.0;
    let secs_per_tick = {
        let raw = target_px / (app.px_per_frame * fps).max(0.0001);
        const STEPS: [f32; 13] = [
            1.0 / 30.0, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 300.0, 600.0,
        ];
        *STEPS.iter().find(|s| **s >= raw).unwrap_or(&600.0)
    };
    let frames_per_tick = (secs_per_tick * fps).max(1.0);

    let first = (app.scroll_x / app.px_per_frame / frames_per_tick).floor() as i64;
    let count = (r.width() / (frames_per_tick * app.px_per_frame)).ceil() as i64 + 2;
    for i in 0..count {
        let f = ((first + i) as f32 * frames_per_tick) as i64;
        if f < 0 {
            continue;
        }
        let x = x_of(app, r, f);
        if x < r.left() - 40.0 || x > r.right() + 40.0 {
            continue;
        }
        painter.line_segment(
            [Pos2::new(x, r.bottom() - 6.0), Pos2::new(x, r.bottom())],
            Stroke::new(1.0, theme::LINE),
        );
        painter.text(
            Pos2::new(x + 4.0, r.top() + 3.0),
            Align2::LEFT_TOP,
            timecode(f, app.fps()),
            theme::mono(10.0),
            theme::TEXT_DIM,
        );
    }
    painter.line_segment(
        [Pos2::new(r.left(), r.bottom()), Pos2::new(r.right(), r.bottom())],
        Stroke::new(1.0, theme::LINE),
    );

    // Dragging anywhere on the ruler scrubs.
    let resp = ui.interact(r, ui.id().with("ruler"), Sense::click_and_drag());
    if resp.drag_started() || resp.clicked() {
        app.scrubbing = true;
    }
    if resp.dragged() || resp.clicked() {
        if let Some(p) = ui.ctx().pointer_latest_pos() {
            let f = frame_at(app, r, p.x).max(0);
            app.set_playhead(f);
        }
    }
    if resp.drag_stopped() {
        app.scrubbing = false;
    }
}

fn draw_track_header(app: &mut App, ui: &mut egui::Ui, tid: TrackId, r: Rect) {
    let painter = ui.painter_at(r);
    let Some(track) = app.project.track(tid) else { return };
    let (name, kind, muted, hidden, locked, height) =
        (track.name.clone(), track.kind, track.muted, track.hidden, track.locked, track.height);

    let bg = if muted || hidden { theme::PANEL } else { theme::PANEL_HI };
    painter.rect_filled(r, CornerRadius::ZERO, bg);
    painter.line_segment(
        [Pos2::new(r.right(), r.top()), Pos2::new(r.right(), r.bottom())],
        Stroke::new(1.0, theme::LINE),
    );
    // A colour flash keyed to the track kind, so video and audio lanes read apart at a glance.
    let flash = if kind == TrackKind::Video { theme::VIDEO_CLIP_HI } else { theme::AUDIO_CLIP_HI };
    painter.rect_filled(
        Rect::from_min_size(r.min, Vec2::new(3.0, r.height())),
        CornerRadius::ZERO,
        if muted || hidden { flash.gamma_multiply(0.35) } else { flash },
    );

    painter.text(
        Pos2::new(r.left() + 12.0, r.top() + 8.0),
        Align2::LEFT_TOP,
        &name,
        theme::ui_font(12.5),
        if muted || hidden { theme::TEXT_FAINT } else { theme::TEXT },
    );

    // Toggles sit on a second line so they never collide with the name on a short track.
    let bs = Vec2::new(26.0, 18.0);
    let by = r.top() + 26.0;
    let mut x = r.left() + 11.0;
    let mut action: Option<u8> = None;

    let mut toggle = |ui: &mut egui::Ui, label: &str, on: bool, x: &mut f32, hint: &str| -> bool {
        let br = Rect::from_min_size(Pos2::new(*x, by), bs);
        *x += bs.x + 5.0;
        if br.bottom() > r.bottom() - 2.0 {
            return false;
        }
        let resp = ui.interact(br, ui.id().with((tid, label)), Sense::click());
        let (fill, fg) = if on {
            (theme::ACCENT_DIM, theme::ACCENT)
        } else if resp.hovered() {
            (theme::RAISED, theme::TEXT)
        } else {
            (theme::PANEL, theme::TEXT_DIM)
        };
        ui.painter().rect_filled(br, CornerRadius::same(3), fill);
        ui.painter().rect_stroke(
            br,
            CornerRadius::same(3),
            Stroke::new(1.0, if on { theme::ACCENT } else { theme::LINE }),
            StrokeKind::Inside,
        );
        ui.painter()
            .text(br.center(), Align2::CENTER_CENTER, label, theme::mono(10.0), fg);
        resp.on_hover_text(hint).clicked()
    };

    if toggle(ui, "M", muted, &mut x, "Mute — leave this track out of playback and export") {
        action = Some(0);
    }
    if kind == TrackKind::Video && toggle(ui, "H", hidden, &mut x, "Hide the picture on this track")
    {
        action = Some(1);
    }
    if toggle(ui, "L", locked, &mut x, "Lock — stop edits on this track") {
        action = Some(2);
    }
    if toggle(ui, "▼", false, &mut x, "Track options") {
        action = Some(3);
    }

    match action {
        Some(0) | Some(1) | Some(2) => {
            app.snapshot();
            if let Some(t) = app.project.track_mut(tid) {
                match action {
                    Some(0) => t.muted = !t.muted,
                    Some(1) => t.hidden = !t.hidden,
                    _ => t.locked = !t.locked,
                }
            }
        }
        Some(3) => app.track_menu = Some(tid),
        _ => {}
    }

    // The options menu, opened from the ▼ button and also by right-clicking the header.
    let menu_resp = ui.interact(r, ui.id().with(("hdr", tid)), Sense::click());
    if menu_resp.secondary_clicked() {
        app.track_menu = Some(tid);
    }
    if app.track_menu == Some(tid) {
        let mut open = true;
        egui::Window::new(format!("Track  {name}"))
            .id(ui.id().with(("trackmenu", tid)))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .fixed_pos(Pos2::new(r.left() + 8.0, r.bottom() + 4.0))
            .show(ui.ctx(), |ui| {
                ui.set_min_width(190.0);
                let mut nm = name.clone();
                if ui
                    .add(egui::TextEdit::singleline(&mut nm).hint_text("Track name"))
                    .changed()
                {
                    if let Some(t) = app.project.track_mut(tid) {
                        t.name = nm;
                    }
                    app.mark_audio_dirty();
                }
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Height").color(theme::TEXT_DIM));
                    if ui.small_button("−").clicked() {
                        app.set_track_height(tid, height - 16.0);
                    }
                    if ui.small_button("+").clicked() {
                        app.set_track_height(tid, height + 16.0);
                    }
                });
                ui.separator();
                if ui.button("Add video track").clicked() {
                    app.add_track(TrackKind::Video);
                    app.track_menu = None;
                }
                if ui.button("Add audio track").clicked() {
                    app.add_track(TrackKind::Audio);
                    app.track_menu = None;
                }
                ui.separator();
                if ui
                    .button(egui::RichText::new("Delete this track").color(theme::BAD))
                    .clicked()
                {
                    app.delete_track(tid);
                    app.track_menu = None;
                }
            });
        if !open {
            app.track_menu = None;
        }
    }
}

/// Returns the clip under the pointer, if any, plus what dragging it would do.
fn draw_track(
    app: &App,
    ui: &egui::Ui,
    painter: &egui::Painter,
    tid: TrackId,
    r: Rect,
) -> Option<(ClipId, TrackId, DragKind)> {
    let Some(track) = app.project.track(tid) else { return None };
    let clip_pad = 2.0;
    let pointer = ui.ctx().pointer_latest_pos();
    let mut hit = None;

    // Only clips whose pixel span intersects the lane are considered.
    let first_frame = frame_at(app, r, r.left()) - 1;
    let last_frame = frame_at(app, r, r.right()) + 1;

    for c in &track.clips {
        if c.end() < first_frame || c.start > last_frame {
            continue;
        }
        let x0 = x_of(app, r, c.start).max(r.left() - 4.0);
        let x1 = x_of(app, r, c.end()).min(r.right() + 4.0);
        if x1 - x0 < 1.0 {
            continue;
        }
        let cr = Rect::from_min_max(
            Pos2::new(x0, r.top() + clip_pad),
            Pos2::new(x1, r.bottom() - clip_pad),
        );
        let (base, hi) = clip_colors(c, track.kind);
        let fill = if c.selected { hi } else { base };
        painter.rect_filled(cr, CornerRadius::same(3), fill);

        // Thumbnails first, so the waveform and label sit on top of them.
        if track.kind == TrackKind::Video && cr.height() >= 34.0 {
            if let Some(mid) = c.media_id() {
                let tw = (cr.height() - 4.0) * app.project.seq().aspect();
                if tw >= 8.0 {
                    let step = thumb_step(app.px_per_frame, tw);
                    let strip = painter.with_clip_rect(cr.shrink(1.0));
                    let (k0, k1) = visible_thumb_range(
                        c.start,
                        c.len,
                        step,
                        tw / app.px_per_frame,
                        first_frame,
                        last_frame,
                    );
                    for k in k0..k1 {
                        let local = k * step;
                        let x = x_of(app, r, c.start + local);
                        if x > cr.right() || x + tw < cr.left() {
                            continue;
                        }
                        let Some(id) = app.thumb(mid, (c.src_in + local).max(0) as u32) else {
                            continue;
                        };
                        let dest = Rect::from_min_size(
                            Pos2::new(x, cr.top() + 2.0),
                            Vec2::new(tw, cr.height() - 4.0),
                        );
                        strip.image(
                            id,
                            dest,
                            Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0)),
                            Color32::WHITE,
                        );
                    }
                    // Keep the name readable over the picture.
                    strip.rect_filled(
                        Rect::from_min_max(cr.min, Pos2::new(cr.right(), cr.top() + 15.0)),
                        CornerRadius::ZERO,
                        Color32::from_black_alpha(120),
                    );
                }
            }
        }

        // Audio waveform, drawn straight from the precomputed envelope.
        if track.kind == TrackKind::Audio || matches!(c.source, ClipSource::Media(_)) {
            if let Some(mid) = c.media_id() {
                if let Some(peaks) = app.peaks_for(mid) {
                    draw_waveform(app, painter, r, cr, c, peaks, track.kind == TrackKind::Audio);
                }
            }
        }

        // A crossfade is drawn as the two clips overlapping in the dissolve window.
        if c.transition_in > 0 {
            let tx = x_of(app, r, c.start + c.transition_in).min(cr.right());
            let band = Rect::from_min_max(Pos2::new(cr.left(), cr.top()), Pos2::new(tx, cr.bottom()));
            if band.width() > 1.0 {
                let p = painter.with_clip_rect(cr);
                p.rect_filled(band, CornerRadius::ZERO, Color32::from_black_alpha(90));
                p.line_segment(
                    [band.left_bottom(), band.right_top()],
                    Stroke::new(1.0, Color32::from_white_alpha(120)),
                );
                p.line_segment(
                    [band.left_top(), band.right_bottom()],
                    Stroke::new(1.0, Color32::from_white_alpha(120)),
                );
            }
        }

        if c.fade_in > 0 {
            let fx = x_of(app, r, c.start + c.fade_in);
            painter.add(egui::Shape::convex_polygon(
                vec![
                    Pos2::new(cr.left(), cr.bottom()),
                    Pos2::new(cr.left(), cr.top()),
                    Pos2::new(fx.min(cr.right()), cr.top()),
                ],
                Color32::from_black_alpha(120),
                Stroke::NONE,
            ));
        }
        if c.fade_out > 0 {
            let fx = x_of(app, r, c.end() - c.fade_out);
            painter.add(egui::Shape::convex_polygon(
                vec![
                    Pos2::new(cr.right(), cr.bottom()),
                    Pos2::new(cr.right(), cr.top()),
                    Pos2::new(fx.max(cr.left()), cr.top()),
                ],
                Color32::from_black_alpha(120),
                Stroke::NONE,
            ));
        }

        let label = match &c.source {
            ClipSource::Media(m) => app
                .project
                .media(*m)
                .map(|m| m.name.clone())
                .unwrap_or_else(|| "missing".into()),
            ClipSource::Text(t) => format!("T  {}", t.text.lines().next().unwrap_or("")),
            ClipSource::Color(_) => "Colour".into(),
        };
        if cr.width() > 28.0 {
            let clip_painter = painter.with_clip_rect(cr.shrink2(Vec2::new(5.0, 0.0)));
            clip_painter.text(
                Pos2::new(cr.left() + 5.0, cr.top() + 2.0),
                Align2::LEFT_TOP,
                label,
                theme::ui_font(10.5),
                Color32::from_white_alpha(215),
            );
        }
        painter.rect_stroke(
            cr,
            CornerRadius::same(3),
            Stroke::new(
                if c.selected { 1.6 } else { 1.0 },
                if c.selected { theme::ACCENT } else { Color32::from_black_alpha(90) },
            ),
            StrokeKind::Inside,
        );

        if let Some(p) = pointer {
            if cr.expand2(Vec2::new(0.0, 0.0)).contains(p) && hit.is_none() {
                let kind = if p.x - cr.left() < EDGE_GRAB {
                    DragKind::TrimStart
                } else if cr.right() - p.x < EDGE_GRAB {
                    DragKind::TrimEnd
                } else {
                    DragKind::Move
                };
                hit = Some((c.id, tid, kind));
            }
        }
    }
    hit
}

fn draw_waveform(
    app: &App,
    painter: &egui::Painter,
    lane: Rect,
    cr: Rect,
    clip: &crate::project::Clip,
    peaks: &[(i16, i16)],
    full_height: bool,
) {
    if peaks.is_empty() || cr.width() < 2.0 {
        return;
    }
    let s = app.project.settings;
    let top = if full_height { cr.top() + 12.0 } else { cr.center().y };
    let bottom = cr.bottom() - 2.0;
    let mid = if full_height { (top + bottom) * 0.5 } else { (cr.center().y + bottom) * 0.5 };
    let half = ((bottom - top) * 0.5).max(1.0);
    if half < 2.0 {
        return;
    }

    let cols = (cr.width().floor() as usize).min(4096);
    let mut shapes = Vec::with_capacity(cols);
    for i in 0..cols {
        let x = cr.left() + i as f32;
        let f0 = frame_at(app, lane, x);
        let f1 = frame_at(app, lane, x + 1.0).max(f0 + 1);
        let src0 = clip.src_in + (f0 - clip.start);
        let src1 = clip.src_in + (f1 - clip.start);
        let b0 = (s.frame_to_sample(src0).max(0) as usize) / PEAK_BUCKET;
        let b1 = ((s.frame_to_sample(src1).max(0) as usize) / PEAK_BUCKET).max(b0 + 1);
        if b0 >= peaks.len() {
            break;
        }
        let end = b1.min(peaks.len());
        let mut lo = 0i32;
        let mut hi = 0i32;
        for p in &peaks[b0..end] {
            lo = lo.min(p.0 as i32);
            hi = hi.max(p.1 as i32);
        }
        let scale = half / 32768.0;
        let y0 = mid - hi as f32 * scale;
        let y1 = mid - lo as f32 * scale;
        shapes.push(egui::Shape::line_segment(
            [Pos2::new(x, y0), Pos2::new(x, y1.max(y0 + 0.6))],
            Stroke::new(1.0, theme::WAVE.gamma_multiply(0.75)),
        ));
    }
    painter.extend(shapes);
}

fn handle_interaction(
    app: &mut App,
    ui: &mut egui::Ui,
    lane: Rect,
    tracks_rect: Rect,
    resp: &egui::Response,
    hovered: Option<(ClipId, TrackId, DragKind)>,
) {
    let pointer = ui.ctx().pointer_latest_pos();

    if let Some((_, _, kind)) = hovered {
        let icon = match kind {
            DragKind::Move => egui::CursorIcon::Grab,
            _ => egui::CursorIcon::ResizeHorizontal,
        };
        ui.ctx().set_cursor_icon(icon);
    }

    // --- start a drag -------------------------------------------------------
    if resp.drag_started() {
        if let (Some((cid, tid, kind)), Some(p)) = (hovered, pointer) {
            if app.project.track(tid).map(|t| t.locked).unwrap_or(false) {
                return;
            }
            if let Some((_, c)) = app.project.clip(cid) {
                let grab = frame_at(app, lane, p.x) - c.start;
                let (orig_start, orig_len, orig_src_in, was_selected) =
                    (c.start, c.len, c.src_in, c.selected);
                if !was_selected {
                    app.project.clear_selection();
                    if let Some(cm) = app.project.clip_mut(cid) {
                        cm.selected = true;
                    }
                }
                // Everything else that is selected travels with the grabbed clip.
                let others: Vec<(ClipId, i64)> = if kind == DragKind::Move {
                    app.project
                        .tracks()
                        .iter()
                        .filter(|t| !t.locked)
                        .flat_map(|t| t.clips.iter())
                        .filter(|o| o.selected && o.id != cid)
                        .map(|o| (o.id, o.start))
                        .collect()
                } else {
                    Vec::new()
                };
                app.snapshot();
                app.drag = Some(DragState {
                    kind,
                    clip: cid,
                    from_track: tid,
                    orig_start,
                    orig_len,
                    orig_src_in,
                    grab,
                    moved: false,
                    others,
                });
            }
        } else if let Some(p) = pointer.filter(|p| tracks_rect.contains(*p)) {
            // Dragging empty space selects a region rather than moving the playhead.
            app.marquee = Some((p, p));
            if !ui.ctx().input(|i| i.modifiers.shift) {
                app.project.clear_selection();
            }
        }
    }

    // --- rubber-band selection ----------------------------------------------
    if resp.dragged() {
        if let (Some((anchor, _)), Some(p)) = (app.marquee, pointer) {
            app.marquee = Some((anchor, p));
            let sel = Rect::from_two_pos(anchor, p);
            let f0 = frame_at(app, lane, sel.left());
            let f1 = frame_at(app, lane, sel.right());
            let additive = ui.ctx().input(|i| i.modifiers.shift);

            let mut top = tracks_rect.top() - app.scroll_y;
            let mut hits: Vec<ClipId> = Vec::new();
            for t in app.project.tracks() {
                let bottom = top + t.height;
                if bottom > sel.top() && top < sel.bottom() && !t.locked {
                    for c in &t.clips {
                        if c.start < f1 && c.end() > f0 {
                            hits.push(c.id);
                        }
                    }
                }
                top = bottom;
            }
            for t in app.project.tracks_mut() {
                for c in &mut t.clips {
                    if hits.contains(&c.id) {
                        c.selected = true;
                    } else if !additive {
                        c.selected = false;
                    }
                }
            }
            ui.painter().rect_filled(sel, CornerRadius::ZERO, theme::ACCENT.gamma_multiply(0.18));
            ui.painter().rect_stroke(
                sel,
                CornerRadius::ZERO,
                Stroke::new(1.0, theme::ACCENT),
                StrokeKind::Inside,
            );
        }
    }

    // --- continue a drag ----------------------------------------------------
    if resp.dragged() {
        if let (Some(st), Some(p)) = (app.drag.as_ref(), pointer) {
            let kind = st.kind;
            let cid = st.clip;
            let grab = st.grab;
            let orig_start = st.orig_start;
            let orig_len = st.orig_len;
            let orig_src_in = st.orig_src_in;
            let from_track = st.from_track;
            let others = st.others.clone();
            let raw = frame_at(app, lane, p.x);

            match kind {
                DragKind::Move => {
                    let want = (raw - grab).max(0);
                    let snapped = app.snap_target(want, Some(cid));
                    // Snapping the tail edge matters as much as the head.
                    let tail = app.snap_target(want + orig_len, Some(cid)) - orig_len;
                    let new_start = if (snapped - want).abs() <= (tail - want).abs() {
                        snapped
                    } else {
                        tail
                    }
                    .max(0);

                    // Nothing may be pushed before the start of the timeline.
                    let min_orig = others
                        .iter()
                        .map(|(_, s)| *s)
                        .chain(std::iter::once(orig_start))
                        .min()
                        .unwrap_or(orig_start);
                    let delta = (new_start - orig_start).max(-min_orig);
                    let new_start = orig_start + delta;

                    for (oid, ostart) in &others {
                        if let Some(oc) = app.project.clip_mut(*oid) {
                            oc.start = ostart + delta;
                        }
                    }

                    let target_track = track_at_y(app, tracks_rect, p.y).unwrap_or(from_track);
                    let same_kind = app
                        .project
                        .track(target_track)
                        .zip(app.project.track(from_track))
                        .map(|(a, b)| a.kind == b.kind && !a.locked)
                        .unwrap_or(false);
                    let dest = if same_kind && others.is_empty() {
                        target_track
                    } else {
                        from_track
                    };

                    if dest != from_track {
                        if let Some(mut c) = take_clip(app, from_track, cid) {
                            c.start = new_start;
                            if let Some(t) = app.project.track_mut(dest) {
                                t.clips.push(c);
                                t.clips.sort_by_key(|c| c.start);
                            }
                            if let Some(s) = app.drag.as_mut() {
                                s.from_track = dest;
                                s.moved = true;
                            }
                        }
                    } else if let Some(c) = app.project.clip_mut(cid) {
                        c.start = new_start;
                    }
                    if let Some(s) = app.drag.as_mut() {
                        s.moved = true;
                    }
                }
                DragKind::TrimStart => {
                    let want = app.snap_target(raw, Some(cid));
                    let max_start = orig_start + orig_len - 1;
                    let speed = app
                        .project
                        .clip(cid)
                        .map(|(_, c)| c.speed.max(0.01) as f64)
                        .unwrap_or(1.0);
                    // Cannot trim earlier than the source has material for.
                    let min_start = orig_start - (orig_src_in as f64 / speed).floor() as i64;
                    let new_start = want.clamp(min_start.max(0), max_start);
                    let delta = new_start - orig_start;
                    if let Some(c) = app.project.clip_mut(cid) {
                        c.start = new_start;
                        c.len = orig_len - delta;
                        c.src_in = orig_src_in + (delta as f64 * speed).round() as i64;
                    }
                    if let Some(s) = app.drag.as_mut() {
                        s.moved = true;
                    }
                }
                DragKind::TrimEnd => {
                    let want = app.snap_target(raw, Some(cid));
                    let media_len = app
                        .project
                        .clip(cid)
                        .and_then(|(_, c)| c.media_id())
                        .and_then(|m| app.project.media(m).map(|m| m.frames))
                        .unwrap_or(i64::MAX / 4);
                    let speed = app
                        .project
                        .clip(cid)
                        .map(|(_, c)| c.speed.max(0.01) as f64)
                        .unwrap_or(1.0);
                    let usable = ((media_len - orig_src_in).max(0) as f64 / speed).floor() as i64;
                    let max_end = orig_start + usable;
                    let new_end = want.clamp(orig_start + 1, max_end.max(orig_start + 1));
                    if let Some(c) = app.project.clip_mut(cid) {
                        c.len = new_end - orig_start;
                    }
                    if let Some(s) = app.drag.as_mut() {
                        s.moved = true;
                    }
                }
            }
        }
    }

    // --- finish -------------------------------------------------------------
    if resp.drag_stopped() {
        app.marquee = None;
        if app.drag.take().is_some() {
            app.project.normalize();
            app.mark_audio_dirty();
        }
    }

    // --- plain click selects ------------------------------------------------
    if resp.clicked() {
        let mods = ui.ctx().input(|i| i.modifiers);
        if let Some((cid, _, _)) = hovered {
            if !mods.command && !mods.shift {
                app.project.clear_selection();
            }
            if let Some(c) = app.project.clip_mut(cid) {
                c.selected = !(mods.command && c.selected);
            }
        } else if let Some(p) = pointer {
            if tracks_rect.contains(p) {
                app.project.clear_selection();
                let f = frame_at(app, lane, p.x).max(0);
                app.set_playhead(f);
            }
        }
    }

    if resp.double_clicked() {
        if let Some(p) = pointer {
            let f = frame_at(app, lane, p.x).max(0);
            app.set_playhead(f);
        }
    }
}

fn track_at_y(app: &App, tracks_rect: Rect, y: f32) -> Option<TrackId> {
    let mut top = tracks_rect.top() - app.scroll_y;
    for t in app.project.tracks() {
        if y >= top && y < top + t.height {
            return Some(t.id);
        }
        top += t.height;
    }
    None
}

fn take_clip(app: &mut App, tid: TrackId, cid: ClipId) -> Option<crate::project::Clip> {
    let t = app.project.track_mut(tid)?;
    let i = t.clips.iter().position(|c| c.id == cid)?;
    Some(t.clips.remove(i))
}
