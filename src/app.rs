//! Application state, transport, editing commands and the non-timeline panels.

use crate::audio::{self, AudioClip, AudioEngine, MixPlan};
use crate::decode::FrameCache;
use crate::export::{self, Encoder, ExportJob, ExportMsg, ExportSettings, Quality};
use crate::ffmpeg::Tools;
use crate::import::{ImportMsg, Importer};
use crate::project::{
    timecode, Clip, ClipId, ClipSource, History, ImportState, MediaId, MediaItem, Project,
    TextProps, Track, TrackId, TrackKind, VideoSettings,
};
use crate::theme;
use egui::{Align2, Color32, Context, CornerRadius, Rect, Stroke, StrokeKind, Vec2};
use memmap2::Mmap;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

pub const PROXY_HEIGHTS: [(u32, &str); 3] =
    [(360, "360p — fastest"), (540, "540p — recommended"), (720, "720p — sharpest")];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DragKind {
    Move,
    TrimStart,
    TrimEnd,
}

pub struct DragState {
    pub kind: DragKind,
    pub clip: ClipId,
    pub from_track: TrackId,
    pub orig_start: i64,
    pub orig_len: i64,
    pub orig_src_in: i64,
    /// Frames between the clip start and where the pointer grabbed it.
    pub grab: i64,
    pub moved: bool,
    /// Every other selected clip, with its start when the drag began, so a multi-clip move keeps
    /// their spacing exactly.
    pub others: Vec<(ClipId, i64)>,
}

pub struct App {
    pub project: Project,
    pub history: History,
    pub project_path: Option<PathBuf>,
    pub dirty: bool,

    pub tools: Arc<Tools>,
    pub importer: Importer,
    pub cache: Arc<FrameCache>,
    pub audio: AudioEngine,

    pub playhead: i64,
    pub playing: bool,
    clock: Option<(Instant, i64)>,

    pub px_per_frame: f32,
    pub scroll_x: f32,
    pub snap: bool,
    pub follow: bool,
    pub proxy_height: u32,

    pub drag: Option<DragState>,
    pub scrubbing: bool,

    tex: Vec<egui::TextureHandle>,
    thumbs: HashMap<(MediaId, u32), egui::TextureHandle>,
    thumb_order: Vec<(MediaId, u32)>,
    pcm: HashMap<MediaId, Arc<Mmap>>,
    peaks: HashMap<MediaId, Arc<Vec<(i16, i16)>>>,
    audio_dirty: bool,

    pub export_job: Option<ExportJob>,
    pub export_pct: f32,
    pub export_note: String,
    pub show_export: bool,
    pub export_settings: ExportSettings,
    pub encoders: Vec<Encoder>,

    pub show_shortcuts: bool,
    pub toast: Option<(String, Instant, bool)>,
    pub selected_media: Option<MediaId>,
    last_edit: Option<ClipId>,
    last_edit_at: Option<Instant>,
    /// Rolling average of frame times, shown in the status bar.
    frame_ms: f32,
}

impl App {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        tools: Arc<Tools>,
        open_path: Option<PathBuf>,
    ) -> Self {
        theme::apply(&cc.egui_ctx);

        let cache_dir = dirs::cache_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("Kite")
            .join("media");
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);

        let importer = Importer::new(tools.clone(), cache_dir, (cores / 2).clamp(1, 4));
        // Roughly a quarter of a small machine's RAM budget, capped so we never page.
        let cache = FrameCache::new(384 * 1024 * 1024, (cores / 2).clamp(1, 3));
        let encoders = export::available_encoders(&tools);

        let project = Project::default();
        let export_settings = ExportSettings {
            path: default_export_path(),
            encoder: Encoder::X264,
            quality: Quality::High,
            width: project.settings.width,
            height: project.settings.height,
            fps: project.settings.fps,
        };

        let mut me = Self {
            project,
            history: History::default(),
            project_path: None,
            dirty: false,
            tools,
            importer,
            cache,
            audio: AudioEngine::new(),
            playhead: 0,
            playing: false,
            clock: None,
            px_per_frame: 4.0,
            scroll_x: 0.0,
            snap: true,
            follow: true,
            proxy_height: 540,
            drag: None,
            scrubbing: false,
            tex: Vec::new(),
            thumbs: HashMap::new(),
            thumb_order: Vec::new(),
            pcm: HashMap::new(),
            peaks: HashMap::new(),
            audio_dirty: true,
            export_job: None,
            export_pct: 0.0,
            export_note: String::new(),
            show_export: false,
            export_settings,
            encoders,
            show_shortcuts: false,
            toast: None,
            selected_media: None,
            last_edit: None,
            last_edit_at: None,
            frame_ms: 0.0,
        };
        if let Some(p) = open_path {
            me.open_path(p);
        }
        me
    }

    // ---------------------------------------------------------------- helpers

    pub fn fps(&self) -> u32 {
        self.project.settings.fps
    }
    pub fn duration(&self) -> i64 {
        self.project.duration()
    }

    pub fn notify(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now(), false));
    }
    pub fn warn(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now(), true));
    }

    pub fn snapshot(&mut self) {
        self.history.push(&self.project);
        self.dirty = true;
        self.audio_dirty = true;
    }

    pub fn peaks_for(&self, id: MediaId) -> Option<&Arc<Vec<(i16, i16)>>> {
        self.peaks.get(&id)
    }

    // ------------------------------------------------------------- transport

    pub fn set_playhead(&mut self, f: i64) {
        let f = f.clamp(0, self.duration().max(0));
        if f != self.playhead {
            self.cache.invalidate_prefetch();
        }
        self.playhead = f;
        let s = self.project.settings.frame_to_sample(f);
        self.audio.set_position_samples(s);
        if self.playing {
            self.clock = Some((Instant::now(), f));
        }
    }

    pub fn toggle_play(&mut self) {
        if self.playing {
            self.stop();
        } else {
            self.play();
        }
    }

    pub fn play(&mut self) {
        if self.duration() == 0 {
            return;
        }
        if self.playhead >= self.duration() {
            self.set_playhead(0);
        }
        self.rebuild_audio_if_needed();
        self.playing = true;
        self.clock = Some((Instant::now(), self.playhead));
        self.audio
            .set_stop_at(self.project.settings.frame_to_sample(self.duration()));
        self.audio
            .set_position_samples(self.project.settings.frame_to_sample(self.playhead));
        self.audio.set_playing(true);
    }

    pub fn stop(&mut self) {
        self.playing = false;
        self.clock = None;
        self.audio.set_playing(false);
    }

    fn advance(&mut self) {
        if !self.playing {
            return;
        }
        let end = self.duration();
        let f = if self.audio.error.is_none() {
            self.project.settings.sample_to_frame(self.audio.position_samples())
        } else if let Some((t0, base)) = self.clock {
            base + (t0.elapsed().as_secs_f64() * self.fps() as f64).round() as i64
        } else {
            self.playhead
        };
        if f >= end {
            self.playhead = end;
            self.stop();
        } else {
            self.playhead = f.max(0);
        }
    }

    // ---------------------------------------------------------------- import

    pub fn import_dialog(&mut self) {
        let files = rfd::FileDialog::new()
            .set_title("Import media")
            .add_filter(
                "Media",
                &[
                    "mp4", "mov", "mkv", "avi", "webm", "m4v", "mts", "wmv", "flv", "mp3", "wav",
                    "aac", "m4a", "flac", "ogg", "opus", "png", "jpg", "jpeg",
                ],
            )
            .add_filter("All files", &["*"])
            .pick_files();
        if let Some(files) = files {
            self.import_paths(files);
        }
    }

    pub fn import_paths(&mut self, paths: Vec<PathBuf>) {
        let mut n = 0;
        for path in paths {
            if !path.is_file() {
                continue;
            }
            if self.project.media.iter().any(|m| m.path == path) {
                continue;
            }
            let id = self.project.alloc_id();
            let name = path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "clip".into());
            self.project.media.push(MediaItem {
                id,
                path: path.clone(),
                name,
                duration: 0.0,
                frames: 0,
                src_width: 0,
                src_height: 0,
                src_fps: 0.0,
                has_video: false,
                has_audio: false,
                proxy_path: None,
                audio_path: None,
                peaks_path: None,
                state: ImportState::Queued,
                error: None,
            });
            self.importer
                .submit(id, path, self.project.settings, self.proxy_height);
            n += 1;
        }
        if n > 0 {
            self.dirty = true;
            self.notify(format!("Importing {n} file{}", if n == 1 { "" } else { "s" }));
        }
    }

    fn poll_import(&mut self) {
        let msgs: Vec<ImportMsg> = self.importer.rx.try_iter().collect();
        for msg in msgs {
            match msg {
                ImportMsg::Probed { id, info, frames } => {
                    if let Some(m) = self.project.media_mut(id) {
                        m.duration = info.duration;
                        m.frames = frames;
                        m.src_width = info.width;
                        m.src_height = info.height;
                        m.src_fps = info.fps;
                        m.has_video = info.has_video;
                        m.has_audio = info.has_audio;
                        m.state = ImportState::Building(0);
                    }
                }
                ImportMsg::Progress { id, pct } => {
                    if let Some(m) = self.project.media_mut(id) {
                        m.state = ImportState::Building(pct);
                    }
                }
                ImportMsg::Ready { id, proxy, audio: apath, peaks, frames } => {
                    if let Some(m) = self.project.media_mut(id) {
                        m.proxy_path = proxy.clone();
                        m.audio_path = apath.clone();
                        m.peaks_path = peaks.clone();
                        m.frames = frames;
                        m.state = ImportState::Ready;
                    }
                    if let Some(p) = proxy {
                        self.cache.register(id, p);
                    }
                    if let Some(p) = apath {
                        if let Some(map) = audio::open_pcm(&p) {
                            self.pcm.insert(id, map);
                        }
                    }
                    if let Some(p) = peaks {
                        if let Some(v) = audio::load_peaks(&p) {
                            self.peaks.insert(id, v);
                        }
                    }
                    self.audio_dirty = true;
                    let name = self
                        .project
                        .media(id)
                        .map(|m| m.name.clone())
                        .unwrap_or_default();
                    self.notify(format!("{name} ready"));
                }
                ImportMsg::Failed { id, error } => {
                    let name = self
                        .project
                        .media(id)
                        .map(|m| m.name.clone())
                        .unwrap_or_default();
                    if let Some(m) = self.project.media_mut(id) {
                        m.state = ImportState::Failed;
                        m.error = Some(error.clone());
                    }
                    self.warn(format!("{name}: {error}"));
                }
            }
        }
    }

    /// Re-derives the audio mix plan from the document. Cheap enough to do on any edit.
    fn rebuild_audio_if_needed(&mut self) {
        if !self.audio_dirty {
            return;
        }
        self.audio_dirty = false;
        let s = self.project.settings;
        let mut clips = Vec::new();
        for track in self.project.tracks.iter().filter(|t| !t.muted) {
            for c in &track.clips {
                let Some(mid) = c.media_id() else { continue };
                let Some(data) = self.pcm.get(&mid) else { continue };
                clips.push(AudioClip {
                    start: s.frame_to_sample(c.start),
                    end: s.frame_to_sample(c.end()),
                    src_offset: s.frame_to_sample(c.src_in),
                    data: data.clone(),
                    volume: c.volume,
                    fade_in: s.frame_to_sample(c.fade_in),
                    fade_out: s.frame_to_sample(c.fade_out),
                });
            }
        }
        let total = s.frame_to_sample(self.project.duration());
        self.audio.set_plan(MixPlan { clips, total });
    }

    // ------------------------------------------------------------ edit verbs

    /// Places a media item on a suitable track at the playhead, pushing to the end if occupied.
    pub fn insert_media(&mut self, id: MediaId) {
        let Some(m) = self.project.media(id).cloned() else { return };
        if !m.is_ready() {
            self.warn("Still importing — one moment");
            return;
        }
        let kind = if m.has_video { TrackKind::Video } else { TrackKind::Audio };
        let track_id = match self
            .project
            .tracks
            .iter()
            .filter(|t| t.kind == kind && !t.locked)
            .min_by_key(|t| t.clips.len())
            .map(|t| t.id)
        {
            Some(t) => t,
            None => self.project.add_track(kind),
        };

        let len = m.frames.max(1);
        let mut start = self.playhead;
        if let Some(t) = self.project.track(track_id) {
            // Avoid dropping on top of an existing clip; append after it instead.
            let clash = t.clips.iter().any(|c| c.start < start + len && c.end() > start);
            if clash {
                start = t.end_frame();
            }
        }

        self.snapshot();
        let clip = self.project.new_clip(ClipSource::Media(id), start, len, 0);
        let cid = clip.id;
        if let Some(t) = self.project.track_mut(track_id) {
            t.clips.push(clip);
            t.normalize();
        }
        self.project.clear_selection();
        if let Some(c) = self.project.clip_mut(cid) {
            c.selected = true;
        }
        self.notify(format!("Added {}", m.name));
    }

    pub fn add_text_clip(&mut self) {
        let fps = self.fps();
        let track_id = match self
            .project
            .tracks
            .iter()
            .find(|t| t.kind == TrackKind::Video && !t.locked)
            .map(|t| t.id)
        {
            Some(t) => t,
            None => self.project.add_track(TrackKind::Video),
        };
        self.snapshot();
        let clip = self.project.new_clip(
            ClipSource::Text(TextProps::default()),
            self.playhead,
            (fps * 3) as i64,
            0,
        );
        let cid = clip.id;
        if let Some(t) = self.project.track_mut(track_id) {
            t.clips.push(clip);
            t.normalize();
        }
        self.project.clear_selection();
        if let Some(c) = self.project.clip_mut(cid) {
            c.selected = true;
        }
        self.notify("Text added — edit it in the inspector");
    }

    /// Razor cut at the playhead across every unlocked track that has a clip there.
    pub fn split_at_playhead(&mut self) {
        let f = self.playhead;
        let targets: Vec<(TrackId, ClipId)> = self
            .project
            .tracks
            .iter()
            .filter(|t| !t.locked)
            .filter_map(|t| {
                t.clip_at(f)
                    .filter(|c| c.start < f && c.end() > f)
                    .map(|c| (t.id, c.id))
            })
            .collect();
        if targets.is_empty() {
            return;
        }
        self.snapshot();
        let mut new_ids = Vec::new();
        for (tid, cid) in targets {
            let mut right = match self.project.clip(cid) {
                Some((_, c)) => c.clone(),
                None => continue,
            };
            let left_len = f - right.start;
            right.id = self.project.alloc_id();
            right.src_in += left_len;
            right.start = f;
            right.len -= left_len;
            right.fade_in = 0;
            new_ids.push(right.id);
            if let Some(t) = self.project.track_mut(tid) {
                if let Some(c) = t.clips.iter_mut().find(|c| c.id == cid) {
                    c.len = left_len;
                    c.fade_out = 0;
                }
                t.clips.push(right);
                t.normalize();
            }
        }
        self.project.clear_selection();
        for id in new_ids {
            if let Some(c) = self.project.clip_mut(id) {
                c.selected = true;
            }
        }
    }

    pub fn delete_selected(&mut self, ripple: bool) {
        let sel = self.project.selected_ids();
        if sel.is_empty() {
            return;
        }
        self.snapshot();
        for track in &mut self.project.tracks {
            if track.locked {
                continue;
            }
            // Ripple pulls later clips back by the removed span, one gap at a time.
            let mut removed: Vec<(i64, i64)> = Vec::new();
            track.clips.retain(|c| {
                if sel.contains(&c.id) {
                    removed.push((c.start, c.len));
                    false
                } else {
                    true
                }
            });
            if ripple {
                removed.sort_by_key(|(s, _)| std::cmp::Reverse(*s));
                for (start, len) in removed {
                    for c in track.clips.iter_mut() {
                        if c.start >= start {
                            c.start -= len;
                        }
                    }
                }
            }
            track.normalize();
        }
        self.notify(if ripple { "Rippled out" } else { "Deleted" });
    }

    /// Copies every selected clip and drops the copies immediately after the selection.
    pub fn duplicate_selected(&mut self) {
        let sel = self.project.selected_ids();
        if sel.is_empty() {
            return;
        }
        self.snapshot();
        let span_end = self
            .project
            .tracks
            .iter()
            .flat_map(|t| t.clips.iter())
            .filter(|c| c.selected)
            .map(|c| c.end())
            .max()
            .unwrap_or(0);
        let span_start = self
            .project
            .tracks
            .iter()
            .flat_map(|t| t.clips.iter())
            .filter(|c| c.selected)
            .map(|c| c.start)
            .min()
            .unwrap_or(0);
        let shift = (span_end - span_start).max(1);

        let track_ids: Vec<TrackId> = self.project.tracks.iter().map(|t| t.id).collect();
        let mut new_ids = Vec::new();
        for tid in track_ids {
            let copies: Vec<Clip> = self
                .project
                .track(tid)
                .map(|t| t.clips.iter().filter(|c| c.selected).cloned().collect())
                .unwrap_or_default();
            for mut c in copies {
                c.id = self.project.alloc_id();
                c.start += shift;
                c.selected = true;
                new_ids.push(c.id);
                if let Some(t) = self.project.track_mut(tid) {
                    t.clips.push(c);
                }
            }
        }
        self.project.clear_selection();
        for id in &new_ids {
            if let Some(c) = self.project.clip_mut(*id) {
                c.selected = true;
            }
        }
        self.project.normalize();
        self.notify(format!("Duplicated {} clip(s)", new_ids.len()));
    }

    /// Moves the selection by whole frames, for fine placement without the mouse.
    pub fn nudge_selected(&mut self, frames: i64) {
        let sel = self.project.selected_ids();
        if sel.is_empty() {
            return;
        }
        let min_start = self
            .project
            .tracks
            .iter()
            .flat_map(|t| t.clips.iter())
            .filter(|c| c.selected)
            .map(|c| c.start)
            .min()
            .unwrap_or(0);
        let delta = frames.max(-min_start);
        if delta == 0 {
            return;
        }
        self.snapshot();
        for t in &mut self.project.tracks {
            if t.locked {
                continue;
            }
            for c in &mut t.clips {
                if c.selected {
                    c.start += delta;
                }
            }
            t.normalize();
        }
    }

    pub fn select_all(&mut self) {
        for t in &mut self.project.tracks {
            for c in &mut t.clips {
                c.selected = true;
            }
        }
    }

    pub fn undo(&mut self) {
        if self.history.undo(&mut self.project) {
            self.audio_dirty = true;
            self.dirty = true;
            self.playhead = self.playhead.min(self.duration());
            self.notify("Undo");
        }
    }
    pub fn redo(&mut self) {
        if self.history.redo(&mut self.project) {
            self.audio_dirty = true;
            self.dirty = true;
            self.playhead = self.playhead.min(self.duration());
            self.notify("Redo");
        }
    }

    /// Nearest edit point for snapping: clip edges on any track, plus the playhead and zero.
    pub fn snap_target(&self, frame: i64, exclude: Option<ClipId>) -> i64 {
        if !self.snap {
            return frame;
        }
        let tol = (8.0 / self.px_per_frame).ceil() as i64 + 1;
        let mut best = frame;
        let mut best_d = tol + 1;
        let mut consider = |p: i64, best: &mut i64, best_d: &mut i64| {
            let d = (p - frame).abs();
            if d <= tol && d < *best_d {
                *best = p;
                *best_d = d;
            }
        };
        consider(0, &mut best, &mut best_d);
        consider(self.playhead, &mut best, &mut best_d);
        for t in &self.project.tracks {
            for c in &t.clips {
                if Some(c.id) == exclude {
                    continue;
                }
                consider(c.start, &mut best, &mut best_d);
                consider(c.end(), &mut best, &mut best_d);
            }
        }
        best
    }


    pub fn thumb(&self, media: MediaId, frame: u32) -> Option<egui::TextureId> {
        self.thumbs.get(&(media, frame)).map(|t| t.id())
    }

    /// Uploads any requested thumbnail that is already decoded, and asks the decoder for a couple
    /// of the missing ones. Deliberately bounded: the timeline must never wait on a decode.
    pub fn upload_thumbs(&mut self, ctx: &Context, wanted: &[(MediaId, u32)]) {
        const MAX_THUMBS: usize = 256;
        const REQUESTS_PER_FRAME: usize = 3;
        let mut requested = 0;

        for &(mid, frame) in wanted {
            if self.thumbs.contains_key(&(mid, frame)) {
                continue;
            }
            match self.cache.peek(mid, frame) {
                Some(img) => {
                    // Thumbnails are small; downsample on upload so VRAM stays modest.
                    let small = downsample(&img, 160);
                    let tex = ctx.load_texture(
                        format!("thumb{mid}_{frame}"),
                        small,
                        egui::TextureOptions::LINEAR,
                    );
                    self.thumbs.insert((mid, frame), tex);
                    self.thumb_order.push((mid, frame));
                }
                None => {
                    if requested < REQUESTS_PER_FRAME {
                        self.cache.prefetch(mid, frame, 1);
                        requested += 1;
                    }
                }
            }
        }

        while self.thumb_order.len() > MAX_THUMBS {
            let k = self.thumb_order.remove(0);
            self.thumbs.remove(&k);
        }
        if requested > 0 {
            ctx.request_repaint_after(std::time::Duration::from_millis(60));
        }
    }

    pub fn drop_thumbs_for(&mut self, media: MediaId) {
        self.thumb_order.retain(|(m, _)| *m != media);
        self.thumbs.retain(|(m, _), _| *m != media);
    }

    pub fn mark_audio_dirty(&mut self) {
        self.audio_dirty = true;
        self.dirty = true;
    }

    /// Applies an inspector edit. Consecutive tweaks to the same clip coalesce into one undo step
    /// so dragging a slider does not fill the history with hundreds of entries.
    pub fn apply_clip_edit(&mut self, cid: ClipId, new: Clip) {
        let coalesce = self.last_edit == Some(cid)
            && self.last_edit_at.map(|t| t.elapsed().as_millis() < 700).unwrap_or(false);
        if !coalesce {
            self.history.push(&self.project);
        }
        self.last_edit = Some(cid);
        self.last_edit_at = Some(Instant::now());
        self.dirty = true;
        self.audio_dirty = true;
        if let Some(c) = self.project.clip_mut(cid) {
            let selected = c.selected;
            *c = new;
            c.selected = selected;
        }
    }

    pub fn remove_media(&mut self, id: MediaId) {
        let used = self
            .project
            .tracks
            .iter()
            .any(|t| t.clips.iter().any(|c| c.media_id() == Some(id)));
        if used {
            self.warn("That clip is still on the timeline");
            return;
        }
        self.snapshot();
        self.project.media.retain(|m| m.id != id);
        self.cache.forget(id);
        self.drop_thumbs_for(id);
        self.pcm.remove(&id);
        self.peaks.remove(&id);
        if self.selected_media == Some(id) {
            self.selected_media = None;
        }
    }

    /// Changing sequence settings invalidates every proxy, because proxies are built at the
    /// project frame rate. Re-import runs in the background; editing stays available throughout.
    pub fn set_sequence(&mut self, s: VideoSettings) {
        let fps_changed = s.fps != self.project.settings.fps;
        self.snapshot();
        self.project.settings = s;
        self.export_settings.width = s.width;
        self.export_settings.height = s.height;
        self.export_settings.fps = s.fps;
        if fps_changed {
            self.rebuild_all_proxies();
            self.notify("Frame rate changed — rebuilding playback files");
        }
    }

    pub fn set_proxy_height(&mut self, h: u32) {
        if h == self.proxy_height {
            return;
        }
        self.proxy_height = h;
        self.rebuild_all_proxies();
        self.notify("Rebuilding playback files at the new quality");
    }

    fn rebuild_all_proxies(&mut self) {
        let items: Vec<(MediaId, PathBuf)> = self
            .project
            .media
            .iter()
            .map(|m| (m.id, m.path.clone()))
            .collect();
        let settings = self.project.settings;
        let ph = self.proxy_height;
        for (id, path) in items {
            if !path.is_file() {
                continue;
            }
            self.cache.forget(id);
            self.drop_thumbs_for(id);
            self.pcm.remove(&id);
            self.peaks.remove(&id);
            if let Some(m) = self.project.media_mut(id) {
                m.state = ImportState::Queued;
            }
            self.importer.submit(id, path, settings, ph);
        }
    }

    // ------------------------------------------------------------ persistence

    pub fn new_project(&mut self) {
        self.project = Project::default();
        self.history = History::default();
        self.project_path = None;
        self.playhead = 0;
        self.dirty = false;
        self.audio_dirty = true;
        self.notify("New project");
    }

    pub fn save(&mut self, save_as: bool) {
        let path = if save_as || self.project_path.is_none() {
            rfd::FileDialog::new()
                .set_title("Save project")
                .add_filter("Kite project", &["kite"])
                .set_file_name(format!("{}.kite", self.project.name))
                .save_file()
        } else {
            self.project_path.clone()
        };
        let Some(path) = path else { return };
        match self.project.save(&path) {
            Ok(()) => {
                if let Some(stem) = path.file_stem() {
                    self.project.name = stem.to_string_lossy().to_string();
                }
                self.project_path = Some(path);
                self.dirty = false;
                self.notify("Project saved");
            }
            Err(e) => self.warn(format!("Could not save: {e}")),
        }
    }

    pub fn open(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .set_title("Open project")
            .add_filter("Kite project", &["kite"])
            .pick_file()
        else {
            return;
        };
        self.open_path(path);
    }

    pub fn open_path(&mut self, path: PathBuf) {
        match Project::load(&path) {
            Ok(p) => {
                self.project = p;
                self.history = History::default();
                self.project_path = Some(path);
                self.playhead = 0;
                self.dirty = false;
                self.pcm.clear();
                self.peaks.clear();
                self.audio_dirty = true;
                // Re-attach derived media; anything missing is rebuilt in the background.
                let items: Vec<MediaItem> = self.project.media.clone();
                for m in items {
                    let ready = m
                        .proxy_path
                        .as_ref()
                        .map(|p| p.is_file())
                        .unwrap_or(!m.has_video);
                    if ready {
                        if let Some(p) = &m.proxy_path {
                            self.cache.register(m.id, p.clone());
                        }
                        if let Some(p) = &m.audio_path {
                            if let Some(map) = audio::open_pcm(p) {
                                self.pcm.insert(m.id, map);
                            }
                        }
                        if let Some(p) = &m.peaks_path {
                            if let Some(v) = audio::load_peaks(p) {
                                self.peaks.insert(m.id, v);
                            }
                        }
                    } else if m.path.is_file() {
                        if let Some(item) = self.project.media_mut(m.id) {
                            item.state = ImportState::Queued;
                        }
                        self.importer.submit(
                            m.id,
                            m.path.clone(),
                            self.project.settings,
                            self.proxy_height,
                        );
                    } else if let Some(item) = self.project.media_mut(m.id) {
                        item.state = ImportState::Failed;
                        item.error = Some("file is missing".into());
                    }
                }
                self.notify("Project opened");
            }
            Err(e) => self.warn(format!("Could not open: {e}")),
        }
    }

    // ---------------------------------------------------------------- export

    pub fn begin_export(&mut self) {
        if self.duration() == 0 {
            self.warn("Nothing on the timeline to export");
            return;
        }
        let unready: Vec<String> = self
            .project
            .media
            .iter()
            .filter(|m| !m.is_ready() && m.state != ImportState::Failed)
            .map(|m| m.name.clone())
            .collect();
        if !unready.is_empty() {
            self.warn(format!("Still importing: {}", unready.join(", ")));
            return;
        }
        self.stop();
        let font = export_font();
        let job = export::start(
            self.tools.clone(),
            self.project.clone(),
            self.export_settings.clone(),
            font,
        );
        self.export_job = Some(job);
        self.export_pct = 0.0;
        self.export_note = "Starting…".into();
        self.show_export = false;
    }

    fn poll_export(&mut self) {
        let Some(job) = &self.export_job else { return };
        let msgs: Vec<ExportMsg> = job.rx.try_iter().collect();
        for m in msgs {
            match m {
                ExportMsg::Progress { pct, frames, speed } => {
                    self.export_pct = pct;
                    self.export_note = format!(
                        "{} of {}   {speed}",
                        timecode(frames, self.fps()),
                        timecode(self.duration(), self.fps())
                    );
                }
                ExportMsg::Done(path) => {
                    self.export_job = None;
                    self.export_pct = 100.0;
                    let p = path.clone();
                    self.notify(format!("Exported to {}", p.display()));
                    reveal(&p);
                }
                ExportMsg::Failed(e) => {
                    self.export_job = None;
                    self.warn(format!("Export failed: {e}"));
                }
            }
        }
    }

    // ------------------------------------------------------------- shortcuts

    fn shortcuts(&mut self, ctx: &Context) {
        if ctx.wants_keyboard_input() {
            return;
        }
        let (keys, mods) = ctx.input(|i| (i.keys_down.clone(), i.modifiers));
        let _ = keys;
        use egui::Key;
        let pressed = |k: Key| ctx.input(|i| i.key_pressed(k));

        let step = if mods.shift { self.fps() as i64 } else { 1 };

        if pressed(Key::Space) {
            self.toggle_play();
        }
        if pressed(Key::ArrowLeft) {
            let p = self.playhead - step;
            self.set_playhead(p);
        }
        if pressed(Key::ArrowRight) {
            let p = self.playhead + step;
            self.set_playhead(p);
        }
        if pressed(Key::Home) {
            self.set_playhead(0);
        }
        if pressed(Key::End) {
            let d = self.duration();
            self.set_playhead(d);
        }
        if pressed(Key::S) && !mods.command {
            self.split_at_playhead();
        }
        if mods.command && pressed(Key::K) {
            self.split_at_playhead();
        }
        if pressed(Key::Delete) || pressed(Key::Backspace) {
            self.delete_selected(mods.shift);
        }
        if mods.command && pressed(Key::Z) {
            if mods.shift {
                self.redo();
            } else {
                self.undo();
            }
        }
        if mods.command && pressed(Key::Y) {
            self.redo();
        }
        if mods.command && pressed(Key::A) {
            self.select_all();
        }
        if mods.command && pressed(Key::D) {
            self.duplicate_selected();
        }
        if pressed(Key::Comma) {
            self.nudge_selected(-step);
        }
        if pressed(Key::Period) {
            self.nudge_selected(step);
        }
        if mods.command && pressed(Key::S) {
            self.save(mods.shift);
        }
        if mods.command && pressed(Key::O) {
            self.open();
        }
        if mods.command && pressed(Key::N) {
            self.new_project();
        }
        if mods.command && pressed(Key::I) {
            self.import_dialog();
        }
        if mods.command && pressed(Key::E) {
            self.show_export = true;
        }
        if pressed(Key::Plus) || pressed(Key::Equals) {
            self.px_per_frame = (self.px_per_frame * 1.35).min(80.0);
        }
        if pressed(Key::Minus) {
            self.px_per_frame = (self.px_per_frame / 1.35).max(0.02);
        }
        if pressed(Key::M) {
            self.snap = !self.snap;
            let s = self.snap;
            self.notify(if s { "Snapping on" } else { "Snapping off" });
        }
    }

    // ------------------------------------------------------------- preview

    fn draw_preview(&mut self, ui: &mut egui::Ui) {
        let avail = ui.available_rect_before_wrap();
        let painter = ui.painter_at(avail);
        painter.rect_filled(avail, CornerRadius::ZERO, theme::BG);

        let aspect = self.project.settings.aspect();
        let mut w = avail.width() - 16.0;
        let mut h = w / aspect;
        if h > avail.height() - 16.0 {
            h = avail.height() - 16.0;
            w = h * aspect;
        }
        if w <= 4.0 || h <= 4.0 {
            return;
        }
        let frame_rect = Rect::from_center_size(avail.center(), Vec2::new(w, h));
        painter.rect_filled(frame_rect, CornerRadius::ZERO, Color32::BLACK);

        let f = self.playhead;
        let mut layer = 0usize;

        // Collect what to draw first so we are not holding a borrow of the project while
        // mutating the texture pool.
        struct Draw {
            media: Option<MediaId>,
            src: u32,
            alpha: f32,
            scale: f32,
            px: f32,
            py: f32,
            color: Option<[u8; 4]>,
            text: Option<TextProps>,
        }
        let mut draws: Vec<Draw> = Vec::new();
        let video: Vec<&Track> = self
            .project
            .tracks
            .iter()
            .filter(|t| t.kind == TrackKind::Video && !t.hidden)
            .collect();
        for track in video.iter().rev() {
            if let Some(c) = track.clip_at(f) {
                let a = c.alpha_at(f);
                match &c.source {
                    ClipSource::Media(m) => draws.push(Draw {
                        media: Some(*m),
                        src: c.source_frame(f).max(0) as u32,
                        alpha: a,
                        scale: c.scale,
                        px: c.pos_x,
                        py: c.pos_y,
                        color: None,
                        text: None,
                    }),
                    ClipSource::Color(rgba) => draws.push(Draw {
                        media: None,
                        src: 0,
                        alpha: a,
                        scale: 1.0,
                        px: 0.0,
                        py: 0.0,
                        color: Some(*rgba),
                        text: None,
                    }),
                    ClipSource::Text(t) => draws.push(Draw {
                        media: None,
                        src: 0,
                        alpha: a,
                        scale: 1.0,
                        px: 0.0,
                        py: 0.0,
                        color: None,
                        text: Some(t.clone()),
                    }),
                }
            }
        }

        for d in draws {
            if let Some(mid) = d.media {
                // During a scrub we take whatever is already decoded rather than stall the frame;
                // the full-quality frame lands a moment later when the pointer settles.
                let frame = if self.scrubbing || self.playing {
                    self.cache
                        .peek(mid, d.src)
                        .or_else(|| self.cache.get(mid, d.src))
                } else {
                    self.cache.get(mid, d.src)
                };
                let Some(img) = frame else { continue };
                let tex = self.texture_slot(ui.ctx(), layer, &img);
                layer += 1;

                let src_aspect = img.width as f32 / img.height.max(1) as f32;
                let mut dw = frame_rect.width();
                let mut dh = dw / src_aspect;
                if dh > frame_rect.height() {
                    dh = frame_rect.height();
                    dw = dh * src_aspect;
                }
                dw *= d.scale;
                dh *= d.scale;
                let center = frame_rect.center()
                    + Vec2::new(d.px * frame_rect.width(), d.py * frame_rect.height());
                let dest = Rect::from_center_size(center, Vec2::new(dw, dh));
                let tint = Color32::from_white_alpha((d.alpha.clamp(0.0, 1.0) * 255.0) as u8);
                painter.with_clip_rect(frame_rect).image(
                    tex,
                    dest,
                    Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    tint,
                );
            } else if let Some(c) = d.color {
                let col = Color32::from_rgba_unmultiplied(
                    c[0],
                    c[1],
                    c[2],
                    (c[3] as f32 * d.alpha) as u8,
                );
                painter.rect_filled(frame_rect, CornerRadius::ZERO, col);
            } else if let Some(t) = d.text {
                let size = (t.size * frame_rect.height()).max(6.0);
                let pos = frame_rect.min
                    + Vec2::new(t.x * frame_rect.width(), t.y * frame_rect.height());
                let anchor = match t.align {
                    crate::project::TextAlign::Left => Align2::LEFT_CENTER,
                    crate::project::TextAlign::Center => Align2::CENTER_CENTER,
                    crate::project::TextAlign::Right => Align2::RIGHT_CENTER,
                };
                let col = Color32::from_rgba_unmultiplied(
                    t.color[0],
                    t.color[1],
                    t.color[2],
                    (t.color[3] as f32 * d.alpha) as u8,
                );
                let font = egui::FontId::proportional(size);
                if t.shadow {
                    painter.with_clip_rect(frame_rect).text(
                        pos + Vec2::new(2.0, 2.0),
                        anchor,
                        &t.text,
                        font.clone(),
                        Color32::from_black_alpha((150.0 * d.alpha) as u8),
                    );
                }
                painter
                    .with_clip_rect(frame_rect)
                    .text(pos, anchor, &t.text, font, col);
            }
        }

        // Warm the frames just ahead of the playhead so playback stays a cache hit.
        if self.playing || self.scrubbing {
            for track in self.project.tracks.iter().filter(|t| t.kind == TrackKind::Video) {
                if let Some(c) = track.clip_at(f) {
                    if let Some(m) = c.media_id() {
                        self.cache.prefetch(m, c.source_frame(f).max(0) as u32 + 1, 12);
                    }
                }
            }
        }

        painter.rect_stroke(
            frame_rect,
            CornerRadius::ZERO,
            Stroke::new(1.0, theme::LINE),
            StrokeKind::Outside,
        );
    }

    fn texture_slot(
        &mut self,
        ctx: &Context,
        idx: usize,
        img: &crate::decode::DecodedFrame,
    ) -> egui::TextureId {
        let color = egui::ColorImage::from_rgba_unmultiplied(
            [img.width as usize, img.height as usize],
            &img.rgba,
        );
        let opts = egui::TextureOptions::LINEAR;
        if idx < self.tex.len() {
            self.tex[idx].set(color, opts);
        } else {
            let h = ctx.load_texture(format!("preview{idx}"), color, opts);
            self.tex.push(h);
        }
        self.tex[idx].id()
    }
}

/// Box-filters a decoded frame down to at most `max_w` wide for use as a timeline thumbnail.
fn downsample(img: &crate::decode::DecodedFrame, max_w: u32) -> egui::ColorImage {
    let (sw, sh) = (img.width.max(1), img.height.max(1));
    if sw <= max_w {
        return egui::ColorImage::from_rgba_unmultiplied([sw as usize, sh as usize], &img.rgba);
    }
    let n = (sw as f32 / max_w as f32).ceil() as u32;
    let dw = (sw / n).max(1);
    let dh = (sh / n).max(1);
    let mut out = vec![0u8; (dw * dh * 4) as usize];
    for y in 0..dh {
        for x in 0..dw {
            let (mut r, mut g, mut b) = (0u32, 0u32, 0u32);
            let mut count = 0u32;
            for sy in 0..n {
                let py = y * n + sy;
                if py >= sh {
                    break;
                }
                for sx in 0..n {
                    let px = x * n + sx;
                    if px >= sw {
                        break;
                    }
                    let i = ((py * sw + px) * 4) as usize;
                    r += img.rgba[i] as u32;
                    g += img.rgba[i + 1] as u32;
                    b += img.rgba[i + 2] as u32;
                    count += 1;
                }
            }
            let count = count.max(1);
            let o = ((y * dw + x) * 4) as usize;
            out[o] = (r / count) as u8;
            out[o + 1] = (g / count) as u8;
            out[o + 2] = (b / count) as u8;
            out[o + 3] = 255;
        }
    }
    egui::ColorImage::from_rgba_unmultiplied([dw as usize, dh as usize], &out)
}

fn default_export_path() -> PathBuf {
    dirs::video_dir()
        .or_else(dirs::desktop_dir)
        .or_else(dirs::home_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("kite-export.mp4")
}

/// A TrueType file for ffmpeg's drawtext. Windows always has these.
fn export_font() -> Option<PathBuf> {
    let candidates = [
        "C:/Windows/Fonts/segoeuib.ttf",
        "C:/Windows/Fonts/seguisb.ttf",
        "C:/Windows/Fonts/segoeui.ttf",
        "C:/Windows/Fonts/arialbd.ttf",
        "C:/Windows/Fonts/arial.ttf",
        "C:/Windows/Fonts/calibrib.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/System/Library/Fonts/Helvetica.ttc",
    ];
    candidates.iter().map(PathBuf::from).find(|p| p.is_file())
}

fn reveal(path: &std::path::Path) {
    #[cfg(windows)]
    {
        let mut c = std::process::Command::new("explorer");
        c.arg("/select,").arg(path);
        let _ = c.spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = path;
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        let t0 = Instant::now();

        self.poll_import();
        self.poll_export();
        self.advance();
        self.rebuild_audio_if_needed();
        self.shortcuts(ctx);

        // Files dropped onto the window import exactly like the menu command.
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if !dropped.is_empty() {
            self.import_paths(dropped);
        }

        let frame_ms = self.frame_ms;
        crate::ui_chrome::top_bar(self, ctx);
        crate::ui_chrome::media_panel(self, ctx);
        crate::ui_chrome::inspector(self, ctx);
        crate::ui_chrome::status_bar(self, ctx, frame_ms);
        crate::timeline::panel(self, ctx);
        crate::ui_chrome::transport(self, ctx);

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(theme::BG))
            .show(ctx, |ui| {
                self.draw_preview(ui);
            });

        crate::ui_chrome::overlays(self, ctx);

        if self.playing {
            // Ask for the next repaint at the frame boundary rather than spinning.
            let d = std::time::Duration::from_secs_f64(1.0 / self.fps().max(1) as f64);
            ctx.request_repaint_after(d);
        } else if self.export_job.is_some()
            || self
                .project
                .media
                .iter()
                .any(|m| !m.is_ready() && m.state != ImportState::Failed)
        {
            ctx.request_repaint_after(std::time::Duration::from_millis(120));
        }

        let ms = t0.elapsed().as_secs_f32() * 1000.0;
        self.frame_ms = self.frame_ms * 0.9 + ms * 0.1;
    }
}

/// Where the caller can find a clip's colour, used by both the timeline and the inspector.
pub fn clip_colors(c: &Clip, kind: TrackKind) -> (Color32, Color32) {
    match (&c.source, kind) {
        (ClipSource::Text(_), _) => (theme::TEXT_CLIP, theme::TEXT_CLIP_HI),
        (ClipSource::Color(_), _) => (theme::TEXT_CLIP, theme::TEXT_CLIP_HI),
        (_, TrackKind::Audio) => (theme::AUDIO_CLIP, theme::AUDIO_CLIP_HI),
        (_, TrackKind::Video) => (theme::VIDEO_CLIP, theme::VIDEO_CLIP_HI),
    }
}

