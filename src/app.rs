//! Application state, transport, editing commands and the non-timeline panels.

use crate::audio::{self, AudioEngine};
use crate::mix::{plan_audio, PcmSource, RetimeCache};
use crate::decode::FrameCache;
use crate::export::{self, Encoder, ExportJob, ExportMsg, ExportSettings, Quality};
use crate::ffmpeg::Tools;
use crate::import::{ImportMsg, Importer};
use crate::proxy::{ProxyBuilder, ProxySource};
use crate::render::{plan_frame, FrameSource, Gpu, Renderer};
use crate::project::{
    timecode, Clip, ClipId, ClipSource, History, ImportState, MediaId, MediaItem, Project,
    RenderItem, RenderStatus, TextProps, TrackId, TrackKind, VideoSettings,
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

/// The preview's end of the render graph.
///
/// It renders on the window's own graphics device, so the composited frame never leaves the GPU:
/// it is handed to egui as a texture rather than read back and re-uploaded.
struct Preview {
    renderer: Renderer,
    state: eframe::egui_wgpu::RenderState,
    /// The registered egui texture and the size it was registered at. The display target is only
    /// rebuilt when the preview changes size, so re-registering is rare.
    registered: Option<(egui::TextureId, u32, u32)>,
}

impl Preview {
    fn new(cc: &eframe::CreationContext<'_>) -> Option<Self> {
        let state = cc.wgpu_render_state.clone()?;
        let gpu = Gpu {
            device: Arc::new(state.device.clone()),
            queue: Arc::new(state.queue.clone()),
            adapter: format!("{} ({:?})", state.adapter.get_info().name, state.adapter.get_info().backend),
        };
        match Renderer::new(gpu) {
            Ok(renderer) => Some(Self { renderer, state, registered: None }),
            Err(e) => {
                eprintln!("the preview renderer could not start: {e:#}");
                None
            }
        }
    }

    fn draw(
        &mut self,
        plan: &crate::render::FramePlan,
        source: &mut dyn FrameSource,
    ) -> Option<egui::TextureId> {
        if let Err(e) = self.renderer.render(plan, source) {
            eprintln!("preview render failed: {e:#}");
            return None;
        }
        let view = self.renderer.to_display_texture()?.clone();
        let size = (plan.width, plan.height);
        match self.registered {
            Some((id, w, h)) if (w, h) == size => Some(id),
            other => {
                if let Some((id, _, _)) = other {
                    self.state.renderer.write().free_texture(&id);
                }
                let id = self.state.renderer.write().register_native_texture(
                    &self.state.device,
                    &view,
                    wgpu::FilterMode::Linear,
                );
                self.registered = Some((id, size.0, size.1));
                Some(id)
            }
        }
    }
}

/// Feeds the mixer from the PCM the importer already extracted and this session has mapped.
struct MappedPcm<'a> {
    pcm: &'a HashMap<MediaId, Arc<Mmap>>,
}

impl PcmSource for MappedPcm<'_> {
    fn pcm(&mut self, media: MediaId) -> Option<Arc<Mmap>> {
        self.pcm.get(&media).cloned()
    }
}

/// Feeds the render graph from the proxy cache.
///
/// During a scrub or playback it takes whatever is already decoded rather than stall the frame,
/// and holds the last good picture while a span is still being prepared instead of going black —
/// which is the behaviour the CPU preview had, kept.
struct CacheFrames<'a> {
    cache: &'a FrameCache,
    impatient: bool,
    preparing: bool,
}

impl FrameSource for CacheFrames<'_> {
    fn frame(
        &mut self,
        _clip: ClipId,
        media: MediaId,
        src_frame: i64,
    ) -> Option<Arc<crate::decode::DecodedFrame>> {
        let f = src_frame.max(0) as u32;
        let exact = if self.impatient {
            self.cache.peek(media, f).or_else(|| self.cache.get(media, f))
        } else {
            self.cache.get(media, f)
        };
        if exact.is_none() {
            self.preparing = true;
        }
        exact.or_else(|| self.cache.last_good(media))
    }
}

/// The sequence a new project will be created with.
#[derive(Clone)]
pub struct NewProject {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Take the sequence from the first clip you add instead of fixing it now.
    pub from_first_clip: bool,
}

impl Default for NewProject {
    fn default() -> Self {
        Self { name: "My video".into(), width: 1920, height: 1080, fps: 30, from_first_clip: true }
    }
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
    pub proxy: Arc<ProxyBuilder>,
    pub audio: AudioEngine,

    pub playhead: i64,
    pub playing: bool,
    clock: Option<(Instant, i64)>,

    pub px_per_frame: f32,
    pub scroll_x: f32,
    pub scroll_y: f32,
    pub marquee: Option<(egui::Pos2, egui::Pos2)>,
    /// Width of the timeline lane as last drawn, so "Fit" has a real number to work from.
    pub lane_width: f32,
    pub snap: bool,
    pub follow: bool,
    pub proxy_height: u32,
    /// False until the sequence has been fixed, either explicitly or by the first clip added.
    pub sequence_locked: bool,

    pub drag: Option<DragState>,
    pub scrubbing: bool,

    preview: Option<Preview>,
    thumbs: HashMap<(MediaId, u32), egui::TextureHandle>,
    thumb_order: Vec<(MediaId, u32)>,
    pcm: HashMap<MediaId, Arc<Mmap>>,
    /// Stretched audio for retimed clips, kept so a plan rebuild on every edit does not redo it.
    retimed: RetimeCache,
    peaks: HashMap<MediaId, Arc<Vec<(i16, i16)>>>,
    audio_dirty: bool,

    pub export_job: Option<ExportJob>,
    /// Which queue row the running export belongs to.
    pub rendering: Option<u64>,
    /// True only while the queue is actually being worked through. Adding a row queues it; it
    /// does not start it, which is how a render queue is expected to behave.
    pub render_running: bool,
    pub editing_output: Option<u64>,
    pub export_pct: f32,
    pub export_note: String,
    pub show_export: bool,
    pub export_settings: ExportSettings,
    pub encoders: Vec<Encoder>,

    pub show_shortcuts: bool,
    pub track_menu: Option<TrackId>,
    pub show_add_track: bool,
    pub show_new_project: bool,
    pub show_new_timeline: bool,
    pub new_timeline_name: String,
    pub new_timeline_custom: bool,
    pub rename_bin: Option<crate::project::BinId>,
    pub show_render_queue: bool,
    pub show_project_manager: bool,
    pub new_proj: NewProject,
    pub toast: Option<(String, Instant, bool)>,
    pub selected_media: Option<MediaId>,
    /// The media pool folder new imports land in and the pool is showing.
    pub active_bin: crate::project::BinId,
    clipboard: Vec<(usize, Clip)>,
    last_autosave: Instant,
    /// Set at startup when an autosave from a previous session is newer than anything saved.
    pub recovery: Option<PathBuf>,
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
        let proxy = ProxyBuilder::new(tools.clone(), (cores / 2).clamp(1, 3));
        // Roughly a quarter of a small machine's RAM budget, capped so we never page.
        let cache = FrameCache::new(384 * 1024 * 1024, (cores / 2).clamp(1, 3), proxy.clone());
        let encoders = export::available_encoders(&tools);
        let preview = Preview::new(cc);

        let project = Project::default();
        let export_settings = ExportSettings {
            path: default_export_path(),
            encoder: Encoder::X264,
            quality: Quality::High,
            width: project.seq().width,
            height: project.seq().height,
            fps: project.seq().fps,
            include_audio: true,
        };

        let mut me = Self {
            project,
            history: History::default(),
            project_path: None,
            dirty: false,
            tools,
            importer,
            cache,
            proxy,
            audio: AudioEngine::new(),
            playhead: 0,
            playing: false,
            clock: None,
            px_per_frame: 4.0,
            scroll_x: 0.0,
            scroll_y: 0.0,
            marquee: None,
            lane_width: 800.0,
            snap: true,
            follow: true,
            proxy_height: 540,
            sequence_locked: false,
            drag: None,
            scrubbing: false,
            preview,
            thumbs: HashMap::new(),
            thumb_order: Vec::new(),
            pcm: HashMap::new(),
            retimed: RetimeCache::default(),
            peaks: HashMap::new(),
            audio_dirty: true,
            export_job: None,
            rendering: None,
            render_running: false,
            editing_output: None,
            export_pct: 0.0,
            export_note: String::new(),
            show_export: false,
            export_settings,
            encoders,
            show_shortcuts: false,
            track_menu: None,
            show_add_track: false,
            show_new_project: false,
            show_new_timeline: false,
            new_timeline_name: "Timeline 2".into(),
            new_timeline_custom: false,
            rename_bin: None,
            show_render_queue: false,
            show_project_manager: false,
            new_proj: NewProject::default(),
            toast: None,
            selected_media: None,
            active_bin: 0,
            clipboard: Vec::new(),
            last_autosave: Instant::now(),
            recovery: None,
            last_edit: None,
            last_edit_at: None,
            frame_ms: 0.0,
        };
        if let Some(p) = open_path {
            me.open_path(p);
        } else {
            // A leftover autosave means the last session ended without saving.
            let a = autosave_path();
            if a.is_file() {
                me.recovery = Some(a);
            } else {
                me.show_project_manager = true;
            }
        }
        me
    }

    // ---------------------------------------------------------------- helpers

    pub fn fps(&self) -> u32 {
        self.project.seq().fps
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
        let s = self.project.seq().frame_to_sample(f);
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
            .set_stop_at(self.project.seq().frame_to_sample(self.duration()));
        self.audio
            .set_position_samples(self.project.seq().frame_to_sample(self.playhead));
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
            self.project.seq().sample_to_frame(self.audio.position_samples())
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
                audio_path: None,
                peaks_path: None,
                state: ImportState::Queued,
                error: None,
                bin: self.active_bin,
            });
            self.importer
                .submit(id, path, self.project.seq().fps, self.proxy_height);
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
                ImportMsg::Probed { id, info, frames, video_dir } => {
                    let path = self.project.media(id).map(|m| m.path.clone());
                    if let Some(m) = self.project.media_mut(id) {
                        m.duration = info.duration;
                        m.frames = frames;
                        m.src_width = info.width;
                        m.src_height = info.height;
                        m.src_fps = info.fps;
                        m.has_video = info.has_video;
                        m.has_audio = info.has_audio;
                        // Editable immediately; the picture fills in as spans are built.
                        m.state = ImportState::Ready;
                    }
                    if !self.sequence_locked
                        && info.has_video
                        && info.width > 0
                        && self.project.tracks().iter().all(|t| t.clips.is_empty())
                    {
                        let fps = if info.fps > 1.0 { info.fps.round() as u32 } else { 30 };
                        let s = VideoSettings { width: info.width, height: info.height, fps };
                        self.project.settings = s;
                        self.export_settings.width = s.width;
                        self.export_settings.height = s.height;
                        self.export_settings.fps = s.fps;
                        self.sequence_locked = true;
                        self.notify(format!(
                            "Sequence set to {}×{} at {} fps to match your footage",
                            s.width, s.height, s.fps
                        ));
                    }
                    if info.has_video {
                        if let Some(path) = path {
                            self.proxy.register(ProxySource {
                                media: id,
                                path,
                                dir: video_dir,
                                fps: self.project.seq().fps,
                                height: self.proxy_height,
                                total_frames: frames.max(1),
                            });
                        }
                    }
                    self.audio_dirty = true;
                    if self.selected_media.is_none() {
                        self.selected_media = Some(id);
                    }
                    let name = self.project.media(id).map(|m| m.name.clone()).unwrap_or_default();
                    self.notify(format!("{name} ready"));
                }
                ImportMsg::AudioReady { id, audio: apath, peaks } => {
                    if let Some(map) = audio::open_pcm(&apath) {
                        self.pcm.insert(id, map);
                    }
                    if let Some(v) = audio::load_peaks(&peaks) {
                        self.peaks.insert(id, v);
                    }
                    if let Some(m) = self.project.media_mut(id) {
                        m.audio_path = Some(apath);
                        m.peaks_path = Some(peaks);
                    }
                    self.audio_dirty = true;
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

    /// Re-derives the audio plan from the document. Cheap enough to do on any edit — except
    /// that a retimed clip is stretched while the plan is built, so this stays off the audio
    /// thread and is only run when something actually changed.
    fn rebuild_audio_if_needed(&mut self) {
        if !self.audio_dirty {
            return;
        }
        self.audio_dirty = false;
        let mut source = MappedPcm { pcm: &self.pcm };
        let plan = plan_audio(&self.project, self.project.tl(), &mut source, &mut self.retimed);
        self.audio.set_plan(plan);
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
            .tracks()
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
            .tracks()
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
            .tracks()
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
        for track in self.project.tracks_mut() {
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
            .tracks()
            .iter()
            .flat_map(|t| t.clips.iter())
            .filter(|c| c.selected)
            .map(|c| c.end())
            .max()
            .unwrap_or(0);
        let span_start = self
            .project
            .tracks()
            .iter()
            .flat_map(|t| t.clips.iter())
            .filter(|c| c.selected)
            .map(|c| c.start)
            .min()
            .unwrap_or(0);
        let shift = (span_end - span_start).max(1);

        let track_ids: Vec<TrackId> = self.project.tracks().iter().map(|t| t.id).collect();
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
            .tracks()
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
        for t in self.project.tracks_mut() {
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

    /// Cross-dissolve each selected clip with the one before it on the same track.
    ///
    /// The dissolve needs material past the earlier clip's out point to fade from, so the length
    /// is clamped to whatever handles that clip actually has.
    pub fn crossfade_selected(&mut self, frames: i64) {
        let sel = self.project.selected_ids();
        if sel.is_empty() {
            self.warn("Select the clip you want to fade into");
            return;
        }
        let mut applied = 0;
        let mut short = 0;
        let mut plan: Vec<(ClipId, i64)> = Vec::new();

        for track in self.project.tracks() {
            for c in track.clips.iter().filter(|c| sel.contains(&c.id)) {
                let Some(prev) = track.prev_clip(c.id) else { continue };
                if prev.end() != c.start {
                    continue; // only clips that actually butt up against each other
                }
                let handles = match prev.media_id().and_then(|m| self.project.media(m)) {
                    Some(m) => (m.frames - (prev.src_in + prev.source_span())).max(0),
                    // Titles and colour cards can be extended freely.
                    None => frames,
                };
                let max = frames.min(handles).min(prev.len).min(c.len);
                if max <= 0 {
                    short += 1;
                    continue;
                }
                if max < frames {
                    short += 1;
                }
                plan.push((c.id, max));
            }
        }

        if plan.is_empty() {
            self.warn("Nothing to fade into — a crossfade needs two clips that touch");
            return;
        }
        self.snapshot();
        for (id, len) in plan {
            if let Some(c) = self.project.clip_mut(id) {
                c.transition_in = len;
            }
            applied += 1;
        }
        if short > 0 {
            self.notify(format!(
                "Added {applied} crossfade(s) — {short} shortened to fit the footage available"
            ));
        } else {
            self.notify(format!("Added {applied} crossfade(s)"));
        }
    }

    /// Changing speed keeps the same stretch of source material and changes how long the clip
    /// occupies the timeline, which is what an editor expects.
    pub fn set_clip_speed(&mut self, cid: ClipId, speed: f32) {
        let speed = speed.clamp(0.1, 8.0);
        let Some((_, c)) = self.project.clip(cid) else { return };
        if (c.speed - speed).abs() < 1e-4 {
            return;
        }
        let span = c.source_span();
        let new_len = ((span as f64) / speed as f64).round().max(1.0) as i64;
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
            c.speed = speed;
            c.len = new_len;
        }
        self.project.normalize();
    }

    pub fn copy_selected(&mut self) {
        let mut out = Vec::new();
        for (ti, track) in self.project.tracks().iter().enumerate() {
            for c in track.clips.iter().filter(|c| c.selected) {
                out.push((ti, c.clone()));
            }
        }
        if out.is_empty() {
            return;
        }
        let n = out.len();
        self.clipboard = out;
        self.notify(format!("Copied {n} clip(s)"));
    }

    /// Pastes the clipboard so its earliest clip lands on the playhead, keeping relative spacing.
    pub fn paste(&mut self) {
        if self.clipboard.is_empty() {
            return;
        }
        let base = self
            .clipboard
            .iter()
            .map(|(_, c)| c.start)
            .min()
            .unwrap_or(0);
        let shift = self.playhead - base;
        self.snapshot();
        let items = self.clipboard.clone();
        let mut new_ids = Vec::new();
        for (ti, mut c) in items {
            let Some(&tid) = self.project.tracks().get(ti).map(|t| &t.id) else { continue };
            if self.project.track(tid).map(|t| t.locked).unwrap_or(true) {
                continue;
            }
            c.id = self.project.alloc_id();
            c.start = (c.start + shift).max(0);
            c.selected = true;
            c.transition_in = 0;
            new_ids.push(c.id);
            if let Some(t) = self.project.track_mut(tid) {
                t.clips.push(c);
            }
        }
        self.project.clear_selection();
        for id in &new_ids {
            if let Some(c) = self.project.clip_mut(*id) {
                c.selected = true;
            }
        }
        self.project.normalize();
        self.notify(format!("Pasted {} clip(s)", new_ids.len()));
    }

    /// Writes a recovery copy periodically so a crash costs seconds, not the session.
    fn autosave(&mut self) {
        if !self.dirty || self.last_autosave.elapsed().as_secs() < 20 {
            return;
        }
        self.last_autosave = Instant::now();
        if let Some(p) = &self.project_path {
            let p = p.clone();
            if self.project.save(&p).is_ok() {
                self.dirty = false;
                self.notify("Saved");
                return;
            }
        }
        self.project.save(&autosave_path()).ok();
    }

    pub fn recover(&mut self) {
        if let Some(p) = self.recovery.take() {
            self.open_path(p);
            self.project_path = None;
            self.dirty = true;
        }
    }
    pub fn discard_recovery(&mut self) {
        if let Some(p) = self.recovery.take() {
            std::fs::remove_file(p).ok();
        }
    }

    pub fn delete_track(&mut self, tid: TrackId) {
        let kind = self.project.track(tid).map(|t| t.kind);
        let remaining = self
            .project
            .tracks()
            .iter()
            .filter(|t| Some(t.kind) == kind)
            .count();
        if remaining <= 1 {
            self.warn("A project needs at least one track of each kind");
            return;
        }
        self.snapshot();
        self.project.tracks_mut().retain(|t| t.id != tid);
        self.notify("Track removed");
    }

    pub fn set_track_height(&mut self, tid: TrackId, h: f32) {
        if let Some(t) = self.project.track_mut(tid) {
            t.height = h.clamp(38.0, 220.0);
        }
        self.dirty = true;
    }

    pub fn add_track(&mut self, kind: TrackKind) {
        self.snapshot();
        self.project.add_track(kind);
    }

    pub fn select_all(&mut self) {
        for t in self.project.tracks_mut() {
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
        for t in self.project.tracks() {
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
            .tracks()
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
        let fps_changed = s.fps != self.project.seq().fps;
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
            self.importer.submit(id, path, settings.fps, ph);
        }
    }

    // ------------------------------------------------------------ persistence

    /// Applies the start dialog and begins a clean project.
    /// Projects on disk in the projects folder, newest first.
    pub fn list_projects() -> Vec<(PathBuf, String, std::time::SystemTime)> {
        let dir = projects_dir();
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().map(|x| x == "kite").unwrap_or(false) {
                    let name = p
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let t = e
                        .metadata()
                        .and_then(|m| m.modified())
                        .unwrap_or(std::time::UNIX_EPOCH);
                    out.push((p, name, t));
                }
            }
        }
        out.sort_by(|a, b| b.2.cmp(&a.2));
        out
    }

    pub fn create_project(&mut self) {
        let d = self.new_proj.clone();
        self.project = Project::default();
        self.project.name = if d.name.trim().is_empty() { "Untitled".into() } else { d.name.clone() };
        if !d.from_first_clip {
            self.project.seq().width = d.width;
            self.project.seq().height = d.height;
            self.project.seq().fps = d.fps;
        }
        self.sequence_locked = !d.from_first_clip;
        self.history = History::default();
        self.project_path = None;
        self.playhead = 0;
        self.dirty = false;
        self.audio_dirty = true;
        self.show_new_project = false;
        self.show_project_manager = false;
        self.active_bin = self.project.root_bin();
        // Give it a home immediately so autosave and the manager have somewhere to put it.
        self.project_path =
            Some(projects_dir().join(format!("{}.kite", sanitise_name(&self.project.name))));
        self.export_settings.width = self.project.seq().width;
        self.export_settings.height = self.project.seq().height;
        self.export_settings.fps = self.project.seq().fps;
    }

    pub fn new_project(&mut self) {
        self.show_new_project = true;
        self.new_proj = NewProject::default();
        self.project = Project::default();
        self.history = History::default();
        self.project_path = None;
        self.playhead = 0;
        self.dirty = false;
        self.audio_dirty = true;
        self.notify("New project");
    }

    /// Saves without asking when the project already has a home, which is what a project manager
    /// implies: projects live in a known folder rather than wherever you last browsed to.
    pub fn save_quiet(&mut self) {
        if self.project_path.is_none() {
            let p = projects_dir().join(format!("{}.kite", sanitise_name(&self.project.name)));
            self.project_path = Some(p);
        }
        if let Some(p) = self.project_path.clone() {
            match self.project.save(&p) {
                Ok(()) => {
                    self.dirty = false;
                    std::fs::remove_file(autosave_path()).ok();
                    self.notify("Project saved");
                }
                Err(e) => self.warn(format!("Could not save: {e}")),
            }
        }
    }

    pub fn save(&mut self, save_as: bool) {
        if !save_as && self.project_path.is_some() {
            self.save_quiet();
            return;
        }
        if !save_as && self.project_path.is_none() {
            self.save_quiet();
            return;
        }
        let path = if save_as || self.project_path.is_none() {
            rfd::FileDialog::new()
                .set_title("Save project")
                .add_filter("Kite project", &["kite"])
                .set_directory(projects_dir())
                .set_file_name(format!("{}.kite", sanitise_name(&self.project.name)))
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
                std::fs::remove_file(autosave_path()).ok();
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
                self.active_bin = self.project.root_bin();
                self.show_project_manager = false;
                self.show_new_project = false;
                // Re-attach derived media; anything missing is rebuilt in the background.
                let items: Vec<MediaItem> = self.project.media.clone();
                for m in items {
                    if m.path.is_file() {
                        if let Some(item) = self.project.media_mut(m.id) {
                            item.state = ImportState::Queued;
                        }
                        // Re-probing is quick and everything derived from it is cached, so this
                        // costs little and cannot disagree with what is actually on disk.
                        self.importer.submit(
                            m.id,
                            m.path.clone(),
                            self.project.seq().fps,
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

    // ---------------------------------------------------------------- render queue

    /// Adds the timeline you are editing to the render queue, the way After Effects adds a comp.
    pub fn add_to_render_queue(&mut self) {
        if self.duration() == 0 {
            self.warn("This timeline is empty — there is nothing to render");
            return;
        }
        let tl = self.project.tl();
        let name = tl.name.clone();
        let tid = tl.id;
        let seq = self.project.seq();
        let out = default_export_dir().join(format!("{}.mp4", sanitise_name(&name)));
        let id = self.project.alloc_id();
        self.project.render_queue.push(RenderItem {
            id,
            timeline: tid,
            output: out,
            width: seq.width,
            height: seq.height,
            fps: seq.fps,
            quality: 0,
            encoder: 0,
            include_audio: true,
            status: RenderStatus::Queued,
            note: String::new(),
        });
        self.dirty = true;
        self.show_render_queue = true;
        self.notify(format!("{name} queued — press Render when you are ready"));
    }

    pub fn queue_item(&self, id: u64) -> Option<&RenderItem> {
        self.project.render_queue.iter().find(|r| r.id == id)
    }

    pub fn settings_for(&self, item: &RenderItem) -> ExportSettings {
        let encoders = &self.encoders;
        ExportSettings {
            path: item.output.clone(),
            encoder: *encoders.get(item.encoder as usize).unwrap_or(&Encoder::X264),
            quality: match item.quality {
                1 => Quality::Balanced,
                2 => Quality::Small,
                _ => Quality::High,
            },
            width: item.width,
            height: item.height,
            fps: item.fps,
            include_audio: item.include_audio,
        }
    }

    /// Starts the first queued row, if nothing is already rendering.
    pub fn pump_render_queue(&mut self) {
        if self.export_job.is_some() || !self.render_running {
            return;
        }
        let next = self
            .project
            .render_queue
            .iter()
            .find(|r| r.status == RenderStatus::Queued)
            .map(|r| (r.id, r.timeline, self.settings_for(r)));
        let Some((id, tid, settings)) = next else {
            // Nothing left to do.
            self.render_running = false;
            self.notify("Render queue finished");
            return;
        };

        let unready: Vec<String> = self
            .project
            .media
            .iter()
            .filter(|m| !m.is_ready() && m.state != ImportState::Failed)
            .map(|m| m.name.clone())
            .collect();
        if !unready.is_empty() {
            self.warn(format!("Still reading: {}", unready.join(", ")));
            return;
        }

        self.stop();
        let job = export::start(self.tools.clone(), self.project.clone(), tid, settings);
        self.export_job = Some(job);
        self.rendering = Some(id);
        self.export_pct = 0.0;
        self.export_note = "Starting…".into();
        if let Some(r) = self.project.render_queue.iter_mut().find(|r| r.id == id) {
            r.status = RenderStatus::Rendering;
        }
    }

    pub fn render_all(&mut self) {
        for r in &mut self.project.render_queue {
            if r.status == RenderStatus::Failed {
                r.status = RenderStatus::Queued;
                r.note.clear();
            }
        }
        if !self.project.render_queue.iter().any(|r| r.status == RenderStatus::Queued) {
            self.warn("Every row is already rendered or switched off");
            return;
        }
        self.render_running = true;
        self.pump_render_queue();
    }

    pub fn stop_rendering(&mut self) {
        self.render_running = false;
        if let Some(j) = &self.export_job {
            j.cancel();
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
        let tl = self.project.tl().id;
        let job = export::start(
            self.tools.clone(),
            self.project.clone(),
            tl,
            self.export_settings.clone(),
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
                ExportMsg::Done { path, width, height, duration, has_audio } => {
                    self.export_job = None;
                    self.export_pct = 100.0;
                    if let Some(rid) = self.rendering.take() {
                        if let Some(r) = self.project.render_queue.iter_mut().find(|r| r.id == rid) {
                            r.status = RenderStatus::Done;
                            r.note = format!(
                                "{width}×{height} · {duration:.1}s · {}",
                                if has_audio { "with sound" } else { "NO SOUND" }
                            );
                        }
                    }
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let sound = if has_audio { "with sound" } else { "NO SOUND" };
                    let msg = format!("Exported {name} — {width}×{height}, {duration:.1}s, {sound}");
                    if has_audio {
                        self.notify(msg);
                    } else {
                        // Saying this loudly beats letting it be discovered on upload.
                        self.warn(msg);
                    }
                    reveal(&path);
                }
                ExportMsg::Failed(e) => {
                    self.export_job = None;
                    if let Some(rid) = self.rendering.take() {
                        if let Some(r) = self.project.render_queue.iter_mut().find(|r| r.id == rid) {
                            r.status = RenderStatus::Failed;
                            r.note = e.clone();
                        }
                    }
                    self.warn(format!("Render failed: {e}"));
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
        if mods.command && pressed(Key::C) {
            self.copy_selected();
        }
        if mods.command && pressed(Key::V) {
            self.paste();
        }
        if mods.command && pressed(Key::T) {
            let half = (self.fps() as i64 / 2).max(1);
            self.crossfade_selected(half);
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
            self.add_to_render_queue();
        }
        if mods.command && pressed(Key::M) {
            self.add_to_render_queue();
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

        let seq = self.project.seq();
        let aspect = seq.aspect();
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

        // Render at the size the preview is actually shown at, never above the sequence, so a
        // small window costs a small render. Everything in a plan is expressed as a fraction of
        // the frame, so the result is the same picture at whatever resolution it is asked for.
        let ppp = ui.ctx().pixels_per_point();
        let rw = even((w * ppp).round() as u32).clamp(16, even(seq.width).max(16));
        let rh = even((rw as f32 / aspect).round() as u32).max(16);

        let f = self.playhead;
        let plan = plan_frame(self.project.tl(), f, rw, rh);
        let mut source = CacheFrames {
            cache: &self.cache,
            impatient: self.scrubbing || self.playing,
            preparing: false,
        };
        let drawn = self
            .preview
            .as_mut()
            .and_then(|p| p.draw(&plan, &mut source));
        let mut preparing = source.preparing;

        match drawn {
            Some(id) => {
                painter.with_clip_rect(frame_rect).image(
                    id,
                    frame_rect,
                    Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            None => {
                preparing = false;
                painter.text(
                    frame_rect.center(),
                    Align2::CENTER_CENTER,
                    "The graphics device could not provide a preview.",
                    theme::ui_font(13.0),
                    theme::TEXT_DIM,
                );
            }
        }

        // Warm the frames just ahead of the playhead so playback stays a cache hit.
        if self.playing || self.scrubbing {
            for track in self.project.tracks().iter().filter(|t| t.kind == TrackKind::Video) {
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

        if preparing {
            let r = Rect::from_min_size(
                egui::pos2(frame_rect.left() + 10.0, frame_rect.top() + 10.0),
                Vec2::new(112.0, 22.0),
            );
            painter.rect_filled(r, CornerRadius::same(3), Color32::from_black_alpha(170));
            painter.text(
                r.center(),
                Align2::CENTER_CENTER,
                "preparing…",
                theme::mono(11.0),
                theme::ACCENT,
            );
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(120));
        }

        if self.project.media.is_empty() {
            let lines = [
                ("Drop a video file anywhere on this window", true),
                ("", false),
                ("or press Ctrl+I to browse", false),
                ("", false),
                ("Then double-click it in the media list to put it on the timeline.", false),
                ("Press S to cut, Shift+Del to remove a bad take, Ctrl+E to export.", false),
            ];
            let mut y = frame_rect.center().y - 54.0;
            for (line, strong) in lines {
                if !line.is_empty() {
                    painter.text(
                        egui::pos2(frame_rect.center().x, y),
                        Align2::CENTER_CENTER,
                        line,
                        if strong { theme::ui_font(16.0) } else { theme::ui_font(12.5) },
                        if strong { theme::TEXT } else { theme::TEXT_DIM },
                    );
                }
                y += if strong { 30.0 } else { 20.0 };
            }
        }
    }
}

/// Render targets want even dimensions; so does the encoder's chroma subsampling.
fn even(v: u32) -> u32 {
    if v < 2 {
        2
    } else {
        v - (v % 2)
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

fn autosave_path() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("Kite")
        .join("recovery.kite")
}

pub fn sanitise_name(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == ' ' { c } else { '-' })
        .collect();
    let s = s.trim().to_string();
    if s.is_empty() { "video".into() } else { s }
}

pub fn default_export_dir() -> PathBuf {
    dirs::video_dir()
        .or_else(dirs::desktop_dir)
        .or_else(dirs::home_dir)
        .unwrap_or_else(std::env::temp_dir)
}

/// Where projects live by default, so the project manager has somewhere to look.
pub fn projects_dir() -> PathBuf {
    let d = dirs::document_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("Kite Projects");
    std::fs::create_dir_all(&d).ok();
    d
}

fn default_export_path() -> PathBuf {
    dirs::video_dir()
        .or_else(dirs::desktop_dir)
        .or_else(dirs::home_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("kite-export.mp4")
}


fn reveal(path: &std::path::Path) {
    #[cfg(windows)]
    {
        // Explorer wants the switch and the path as one token; passing them separately opens the
        // documents folder instead of selecting the file.
        use std::os::windows::process::CommandExt;
        let mut c = std::process::Command::new("explorer.exe");
        c.raw_arg(format!("/select,\"{}\"", path.display()));
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
        self.pump_render_queue();
        self.autosave();
        // Keep the pool pointed at a bin that exists.
        if !self.project.bins.iter().any(|b| b.id == self.active_bin) {
            self.active_bin = self.project.root_bin();
        }
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

