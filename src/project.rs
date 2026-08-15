//! The document model.
//!
//! Timeline positions are integer **frames at the project frame rate**. Proxies are generated at
//! that same rate, so a clip's source in-point is also measured in frames and no floating point
//! drift can accumulate across edits. Audio is addressed in samples, derived exactly from frames.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const SAMPLE_RATE: u32 = 48_000;

pub type MediaId = u64;
pub type ClipId = u64;
pub type TrackId = u64;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct VideoSettings {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl Default for VideoSettings {
    fn default() -> Self {
        Self { width: 1920, height: 1080, fps: 30 }
    }
}

impl VideoSettings {
    pub fn frames_to_secs(&self, f: i64) -> f64 {
        f as f64 / self.fps as f64
    }
    pub fn secs_to_frames(&self, s: f64) -> i64 {
        (s * self.fps as f64).round() as i64
    }
    pub fn frame_to_sample(&self, f: i64) -> i64 {
        (f as i128 * SAMPLE_RATE as i128 / self.fps as i128) as i64
    }
    pub fn sample_to_frame(&self, s: i64) -> i64 {
        (s as i128 * self.fps as i128 / SAMPLE_RATE as i128) as i64
    }
    pub fn aspect(&self) -> f32 {
        self.width as f32 / self.height.max(1) as f32
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImportState {
    Queued,
    Probing,
    /// Proxy + audio are being generated; `pct` is 0..100.
    Building(u8),
    Ready,
    Failed,
}

impl ImportState {
    pub fn is_ready(&self) -> bool {
        matches!(self, ImportState::Ready)
    }
}

/// One item in the media pool: a source file plus everything we derived from it on import.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MediaItem {
    pub id: MediaId,
    pub path: PathBuf,
    pub name: String,
    /// Source duration in seconds, as probed.
    pub duration: f64,
    /// Duration in project frames, which is what the timeline uses.
    pub frames: i64,
    pub src_width: u32,
    pub src_height: u32,
    pub src_fps: f64,
    pub has_video: bool,
    pub has_audio: bool,
    /// Our indexed all-intra frame store. Random access is an offset lookup.
    pub proxy_path: Option<PathBuf>,
    /// Interleaved stereo i16 at 48 kHz, memory-mapped at playback time.
    pub audio_path: Option<PathBuf>,
    /// Precomputed min/max envelope for instant waveform drawing.
    pub peaks_path: Option<PathBuf>,
    pub state: ImportState,
    pub error: Option<String>,
}

impl MediaItem {
    pub fn is_ready(&self) -> bool {
        self.state.is_ready()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrackKind {
    Video,
    Audio,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextAlign {
    Left,
    Center,
    Right,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TextProps {
    pub text: String,
    /// Size as a fraction of frame height, so it survives resolution changes.
    pub size: f32,
    pub color: [u8; 4],
    pub align: TextAlign,
    pub bold: bool,
    /// Position in normalised frame coordinates, 0..1, of the text anchor.
    pub x: f32,
    pub y: f32,
    pub shadow: bool,
    pub box_bg: bool,
}

impl Default for TextProps {
    fn default() -> Self {
        Self {
            text: "Your text here".into(),
            size: 0.09,
            color: [255, 255, 255, 255],
            align: TextAlign::Center,
            bold: true,
            x: 0.5,
            y: 0.82,
            shadow: true,
            box_bg: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ClipSource {
    Media(MediaId),
    Text(TextProps),
    /// A solid colour card, handy for intros and letterboxing.
    Color([u8; 4]),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Clip {
    pub id: ClipId,
    pub source: ClipSource,
    /// Timeline position of the clip's first frame.
    pub start: i64,
    /// Length on the timeline, in frames. Always >= 1.
    pub len: i64,
    /// Offset into the source, in frames. Zero for generated sources.
    pub src_in: i64,
    pub volume: f32,
    pub opacity: f32,
    /// Uniform scale multiplier applied on top of the fit-to-frame default.
    pub scale: f32,
    /// Translation in normalised frame units (0.5 = half a frame width).
    pub pos_x: f32,
    pub pos_y: f32,
    pub fade_in: i64,
    pub fade_out: i64,
    pub selected: bool,
}

impl Clip {
    pub fn end(&self) -> i64 {
        self.start + self.len
    }
    pub fn contains(&self, f: i64) -> bool {
        f >= self.start && f < self.end()
    }
    /// Source frame that should be shown for timeline frame `f`.
    pub fn source_frame(&self, f: i64) -> i64 {
        self.src_in + (f - self.start)
    }
    pub fn gain_at(&self, f: i64) -> f32 {
        let local = f - self.start;
        let mut g = self.volume;
        if self.fade_in > 0 && local < self.fade_in {
            g *= local as f32 / self.fade_in as f32;
        }
        if self.fade_out > 0 && local >= self.len - self.fade_out {
            let r = (self.len - local) as f32 / self.fade_out as f32;
            g *= r.clamp(0.0, 1.0);
        }
        g
    }
    pub fn alpha_at(&self, f: i64) -> f32 {
        let local = f - self.start;
        let mut a = self.opacity;
        if self.fade_in > 0 && local < self.fade_in {
            a *= local as f32 / self.fade_in as f32;
        }
        if self.fade_out > 0 && local >= self.len - self.fade_out {
            let r = (self.len - local) as f32 / self.fade_out as f32;
            a *= r.clamp(0.0, 1.0);
        }
        a
    }
    pub fn media_id(&self) -> Option<MediaId> {
        match &self.source {
            ClipSource::Media(m) => Some(*m),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Track {
    pub id: TrackId,
    pub kind: TrackKind,
    pub name: String,
    pub muted: bool,
    pub hidden: bool,
    pub locked: bool,
    pub height: f32,
    /// Kept sorted by `start`; `normalize` restores the invariant after edits.
    pub clips: Vec<Clip>,
}

impl Track {
    pub fn new(id: TrackId, kind: TrackKind, name: impl Into<String>) -> Self {
        Self {
            id,
            kind,
            name: name.into(),
            muted: false,
            hidden: false,
            locked: false,
            height: if kind == TrackKind::Video { 64.0 } else { 48.0 },
            clips: Vec::new(),
        }
    }

    pub fn clip_at(&self, f: i64) -> Option<&Clip> {
        // Clips are sorted and non-overlapping, so a binary search is exact.
        let i = self.clips.partition_point(|c| c.end() <= f);
        self.clips.get(i).filter(|c| c.contains(f))
    }

    pub fn normalize(&mut self) {
        self.clips.sort_by_key(|c| c.start);
        // Trim any overlap left behind by a drag; the later clip wins its start position.
        for i in 1..self.clips.len() {
            let prev_end = self.clips[i - 1].end();
            let start = self.clips[i].start;
            if prev_end > start {
                let over = prev_end - start;
                let p = &mut self.clips[i - 1];
                p.len = (p.len - over).max(1);
            }
        }
        self.clips.retain(|c| c.len > 0);
    }

    pub fn end_frame(&self) -> i64 {
        self.clips.last().map(|c| c.end()).unwrap_or(0)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Project {
    pub version: u32,
    pub name: String,
    pub settings: VideoSettings,
    pub media: Vec<MediaItem>,
    pub tracks: Vec<Track>,
    next_id: u64,
}

impl Default for Project {
    fn default() -> Self {
        let mut p = Self {
            version: 1,
            name: "Untitled".into(),
            settings: VideoSettings::default(),
            media: Vec::new(),
            tracks: Vec::new(),
            next_id: 1,
        };
        let v2 = p.alloc_id();
        let v1 = p.alloc_id();
        let a1 = p.alloc_id();
        p.tracks.push(Track::new(v2, TrackKind::Video, "V2"));
        p.tracks.push(Track::new(v1, TrackKind::Video, "V1"));
        p.tracks.push(Track::new(a1, TrackKind::Audio, "A1"));
        p
    }
}

impl Project {
    pub fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn media(&self, id: MediaId) -> Option<&MediaItem> {
        self.media.iter().find(|m| m.id == id)
    }
    pub fn media_mut(&mut self, id: MediaId) -> Option<&mut MediaItem> {
        self.media.iter_mut().find(|m| m.id == id)
    }
    pub fn track(&self, id: TrackId) -> Option<&Track> {
        self.tracks.iter().find(|t| t.id == id)
    }
    pub fn track_mut(&mut self, id: TrackId) -> Option<&mut Track> {
        self.tracks.iter_mut().find(|t| t.id == id)
    }

    /// Video tracks are stored top-first, which is also compositing order (last drawn wins).
    pub fn video_tracks(&self) -> impl Iterator<Item = &Track> {
        self.tracks.iter().filter(|t| t.kind == TrackKind::Video)
    }
    pub fn audio_tracks(&self) -> impl Iterator<Item = &Track> {
        self.tracks.iter().filter(|t| t.kind == TrackKind::Audio)
    }

    pub fn duration(&self) -> i64 {
        self.tracks.iter().map(|t| t.end_frame()).max().unwrap_or(0)
    }

    pub fn clip(&self, id: ClipId) -> Option<(&Track, &Clip)> {
        for t in &self.tracks {
            if let Some(c) = t.clips.iter().find(|c| c.id == id) {
                return Some((t, c));
            }
        }
        None
    }
    pub fn clip_mut(&mut self, id: ClipId) -> Option<&mut Clip> {
        for t in &mut self.tracks {
            if let Some(c) = t.clips.iter_mut().find(|c| c.id == id) {
                return Some(c);
            }
        }
        None
    }
    pub fn track_of_clip(&self, id: ClipId) -> Option<TrackId> {
        self.tracks
            .iter()
            .find(|t| t.clips.iter().any(|c| c.id == id))
            .map(|t| t.id)
    }

    pub fn selected_ids(&self) -> Vec<ClipId> {
        self.tracks
            .iter()
            .flat_map(|t| t.clips.iter())
            .filter(|c| c.selected)
            .map(|c| c.id)
            .collect()
    }
    pub fn clear_selection(&mut self) {
        for t in &mut self.tracks {
            for c in &mut t.clips {
                c.selected = false;
            }
        }
    }

    pub fn new_clip(&mut self, source: ClipSource, start: i64, len: i64, src_in: i64) -> Clip {
        Clip {
            id: self.alloc_id(),
            source,
            start,
            len: len.max(1),
            src_in,
            volume: 1.0,
            opacity: 1.0,
            scale: 1.0,
            pos_x: 0.0,
            pos_y: 0.0,
            fade_in: 0,
            fade_out: 0,
            selected: false,
        }
    }

    /// First free frame on a track at or after `from`, used for append-style insertion.
    pub fn append_point(&self, track: TrackId) -> i64 {
        self.track(track).map(|t| t.end_frame()).unwrap_or(0)
    }

    pub fn add_track(&mut self, kind: TrackKind) -> TrackId {
        let id = self.alloc_id();
        let n = self.tracks.iter().filter(|t| t.kind == kind).count() + 1;
        let name = match kind {
            TrackKind::Video => format!("V{n}"),
            TrackKind::Audio => format!("A{n}"),
        };
        let t = Track::new(id, kind, name);
        // Video tracks stack upward, so new video goes on top; audio appends below.
        match kind {
            TrackKind::Video => self.tracks.insert(0, t),
            TrackKind::Audio => self.tracks.push(t),
        }
        id
    }

    pub fn normalize(&mut self) {
        for t in &mut self.tracks {
            t.normalize();
        }
    }

    pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        // Write to a sibling temp file first so a crash mid-save cannot destroy the project.
        let tmp = path.with_extension("kite.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)?;
        let mut p: Project = serde_json::from_str(&s)?;
        p.normalize();
        Ok(p)
    }
}

/// Snapshot undo. The document is small enough that cloning it is cheaper, and far more robust,
/// than maintaining inverse operations for every edit.
pub struct History {
    past: Vec<Project>,
    future: Vec<Project>,
    limit: usize,
}

impl Default for History {
    fn default() -> Self {
        Self { past: Vec::new(), future: Vec::new(), limit: 200 }
    }
}

impl History {
    pub fn push(&mut self, p: &Project) {
        self.past.push(p.clone());
        if self.past.len() > self.limit {
            self.past.remove(0);
        }
        self.future.clear();
    }
    pub fn undo(&mut self, cur: &mut Project) -> bool {
        if let Some(prev) = self.past.pop() {
            self.future.push(std::mem::replace(cur, prev));
            true
        } else {
            false
        }
    }
    pub fn redo(&mut self, cur: &mut Project) -> bool {
        if let Some(next) = self.future.pop() {
            self.past.push(std::mem::replace(cur, next));
            true
        } else {
            false
        }
    }
    pub fn can_undo(&self) -> bool {
        !self.past.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.future.is_empty()
    }
}

pub fn timecode(frames: i64, fps: u32) -> String {
    let fps = fps.max(1) as i64;
    let neg = frames < 0;
    let f = frames.abs();
    let total_secs = f / fps;
    let ff = f % fps;
    let s = total_secs % 60;
    let m = (total_secs / 60) % 60;
    let h = total_secs / 3600;
    let sign = if neg { "-" } else { "" };
    format!("{sign}{h:02}:{m:02}:{s:02}:{ff:02}")
}
