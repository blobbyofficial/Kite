//! Export: renders the timeline through the GPU graph, frame by frame, and hands raw pixels to
//! ffmpeg to encode.
//!
//! Playback uses proxies, but delivery never does — the decoders below read the source files at
//! full resolution, so what you export is the real quality regardless of what you edited against.
//!
//! Compositing is **not** here. It is in `render.rs`, shared with the preview, which is the whole
//! point of the arrangement: ffmpeg does demux, decode and encode, and nothing else.

use crate::decode::DecodedFrame;
use crate::ffmpeg::{self, Tools};
use crate::project::{ClipId, MediaId, Project, Timeline};
use crate::render::{plan_frame, FrameSource, Gpu, Renderer};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Encoder {
    X264,
    NvencH264,
    QsvH264,
    AmfH264,
}

impl Encoder {
    pub fn label(&self) -> &'static str {
        match self {
            Encoder::X264 => "x264 (software, works everywhere)",
            Encoder::NvencH264 => "NVIDIA NVENC (fast)",
            Encoder::QsvH264 => "Intel Quick Sync (fast)",
            Encoder::AmfH264 => "AMD AMF (fast)",
        }
    }
    fn name(&self) -> &'static str {
        match self {
            Encoder::X264 => "libx264",
            Encoder::NvencH264 => "h264_nvenc",
            Encoder::QsvH264 => "h264_qsv",
            Encoder::AmfH264 => "h264_amf",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Quality {
    High,
    Balanced,
    Small,
}

impl Quality {
    pub fn label(&self) -> &'static str {
        match self {
            Quality::High => "High — best for YouTube",
            Quality::Balanced => "Balanced",
            Quality::Small => "Small file",
        }
    }
    fn crf(&self) -> u32 {
        match self {
            Quality::High => 18,
            Quality::Balanced => 21,
            Quality::Small => 25,
        }
    }
    fn audio_kbps(&self) -> u32 {
        match self {
            Quality::High => 320,
            Quality::Balanced => 192,
            Quality::Small => 128,
        }
    }
}

#[derive(Clone)]
pub struct ExportSettings {
    pub path: PathBuf,
    pub encoder: Encoder,
    pub quality: Quality,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub include_audio: bool,
}

#[derive(Debug)]
pub enum ExportMsg {
    Progress { pct: f32, frames: i64, speed: String },
    /// Carries what the finished file actually contains, read back from the file itself rather
    /// than assumed from what we asked for.
    Done { path: PathBuf, width: u32, height: u32, duration: f64, has_audio: bool },
    Failed(String),
}

pub struct ExportJob {
    pub rx: crossbeam_channel::Receiver<ExportMsg>,
    cancel: Arc<AtomicBool>,
    pub settings: ExportSettings,
}

impl ExportJob {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// What sound, if any, the current timeline would contribute to an export. Shown in the export
/// dialog so "why is there no audio" is answered before rendering rather than after.
pub fn audio_summary(project: &Project, tl: &Timeline) -> (usize, usize, bool) {
    let mut clips = 0;
    let mut silent = 0;
    let mut muted_track_has_audio = false;
    for track in &tl.tracks {
        for c in &track.clips {
            let has = c
                .media_id()
                .and_then(|m| project.media(m))
                .map(|m| m.has_audio)
                .unwrap_or(false);
            if !has {
                continue;
            }
            if track.muted {
                muted_track_has_audio = true;
                continue;
            }
            clips += 1;
            if c.volume <= 0.0001 {
                silent += 1;
            }
        }
    }
    (clips, silent, muted_track_has_audio)
}

/// Asks ffmpeg which encoders this build actually has, so we only offer ones that will work.
pub fn available_encoders(tools: &Tools) -> Vec<Encoder> {
    let mut out = vec![Encoder::X264];
    let Ok(res) = ffmpeg::command(&tools.ffmpeg)
        .args(["-v", "quiet", "-encoders"])
        .stdout(Stdio::piped())
        .output()
    else {
        return out;
    };
    let text = String::from_utf8_lossy(&res.stdout);
    for (enc, needle) in [
        (Encoder::NvencH264, "h264_nvenc"),
        (Encoder::QsvH264, "h264_qsv"),
        (Encoder::AmfH264, "h264_amf"),
    ] {
        if text.contains(needle) {
            out.push(enc);
        }
    }
    out
}

/// How this ffmpeg build accepts a filtergraph that is too large to pass as an argument.
///
/// `-filter_complex_script` was the long-standing spelling; ffmpeg 7 introduced the generic
/// `-/option file` form and ffmpeg 8 removed the old one. Bundled builds move, so rather than
/// guess from a version banner we ask ffmpeg once and remember the answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GraphArg {
    /// `-/filter_complex file` — ffmpeg 7 and later.
    SlashFile,
    /// `-filter_complex_script file` — ffmpeg 6 and earlier.
    ScriptFile,
    /// Neither worked; pass it inline and hope it fits the command line.
    Inline,
}

static GRAPH_ARG: std::sync::OnceLock<GraphArg> = std::sync::OnceLock::new();

pub fn graph_arg(tools: &Tools) -> GraphArg {
    *GRAPH_ARG.get_or_init(|| detect_graph_arg(tools))
}

fn detect_graph_arg(tools: &Tools) -> GraphArg {
    let dir = std::env::temp_dir().join(format!("kite-probe-{}", std::process::id()));
    if std::fs::create_dir_all(&dir).is_err() {
        return GraphArg::Inline;
    }
    let name = "probe_graph.txt";
    if std::fs::write(dir.join(name), b"[0:v]null[vout]").is_err() {
        std::fs::remove_dir_all(&dir).ok();
        return GraphArg::Inline;
    }

    let mut found = GraphArg::Inline;
    for style in [GraphArg::SlashFile, GraphArg::ScriptFile] {
        let mut cmd = ffmpeg::command(&tools.ffmpeg);
        cmd.current_dir(&dir);
        cmd.args(["-v", "error", "-f", "lavfi", "-i", "color=c=black:s=32x32:d=0.1"]);
        match style {
            GraphArg::SlashFile => cmd.args(["-/filter_complex", name]),
            GraphArg::ScriptFile => cmd.args(["-filter_complex_script", name]),
            GraphArg::Inline => unreachable!(),
        };
        cmd.args(["-map", "[vout]", "-frames:v", "1", "-f", "null", "-"]);
        let ok = cmd
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            found = style;
            break;
        }
    }
    std::fs::remove_dir_all(&dir).ok();
    found
}


fn f(t: f64) -> String {
    format!("{t:.6}")
}

struct Graph {
    parts: Vec<String>,
    n: usize,
}

impl Graph {
    fn new() -> Self {
        Self { parts: Vec::new(), n: 0 }
    }
    fn label(&mut self, prefix: &str) -> String {
        self.n += 1;
        format!("{prefix}{}", self.n)
    }
    fn push(&mut self, s: String) {
        self.parts.push(s);
    }
    fn join(&self) -> String {
        self.parts.join(";")
    }
}

/// Builds the audio side of the render: the ordered list of source files to open and the
/// filtergraph that trims, retimes, fades and mixes them.
///
/// Video is not in here any more. The picture is composited on the GPU and arrives at ffmpeg as
/// raw frames on stdin, which is always input 0 — hence `first_input`, the index the first audio
/// file gets on the command line.
pub fn build_audio_graph(
    project: &Project,
    tl: &Timeline,
    settings: &ExportSettings,
    first_input: usize,
) -> (Vec<PathBuf>, String, bool) {
    let fps = settings.fps.max(1);
    let mut inputs: Vec<PathBuf> = Vec::new();
    let mut index_of: BTreeMap<MediaId, usize> = BTreeMap::new();
    if settings.include_audio {
        for track in tl.tracks.iter().filter(|t| !t.muted) {
            for clip in &track.clips {
                let Some(mid) = clip.media_id() else { continue };
                let Some(m) = project.media(mid) else { continue };
                if !m.has_audio || index_of.contains_key(&mid) {
                    continue;
                }
                index_of.insert(mid, first_input + inputs.len());
                inputs.push(m.path.clone());
            }
        }
    }

    let mut g = Graph::new();
    let mut alabels: Vec<String> = Vec::new();
    for track in tl.tracks.iter().filter(|t| !t.muted && settings.include_audio) {
        for (i, clip) in track.clips.iter().enumerate() {
            // If the next clip dissolves in, this one keeps rolling underneath for that long.
            let tail = track
                .clips
                .get(i + 1)
                .map(|n| n.transition_in.max(0))
                .unwrap_or(0);
            let Some(mid) = clip.media_id() else { continue };
            let Some(m) = project.media(mid) else { continue };
            if !m.has_audio {
                continue;
            }
            let Some(&idx) = index_of.get(&mid) else { continue };
            let speed = clip.speed.max(0.01) as f64;
            let t0 = clip.start as f64 / fps as f64;
            let src_in = clip.src_in as f64 / fps as f64;
            let src_out = src_in + ((clip.len + tail) as f64 * speed / fps as f64);
            let delay_ms = (t0 * 1000.0).round() as i64;
            let lbl = g.label("a");
            let mut chain = format!(
                "[{idx}:a]atrim=start={}:end={},asetpts=PTS-STARTPTS,\
                 aformat=sample_fmts=fltp:sample_rates=48000:channel_layouts=stereo",
                f(src_in),
                f(src_out)
            );
            for step in atempo_chain(speed) {
                chain.push_str(&format!(",atempo={}", f(step)));
            }
            if (clip.volume - 1.0).abs() > 0.001 {
                chain.push_str(&format!(",volume={:.4}", clip.volume));
            }
            if clip.fade_in > 0 {
                chain.push_str(&format!(
                    ",afade=t=in:st=0:d={}",
                    f(clip.fade_in as f64 / fps as f64)
                ));
            }
            let played = (clip.len + tail) as f64 / fps as f64;
            if clip.fade_out > 0 {
                let d = clip.fade_out as f64 / fps as f64;
                chain.push_str(&format!(
                    ",afade=t=out:st={}:d={}",
                    f(played - tail as f64 / fps as f64 - d),
                    f(d)
                ));
            }
            if clip.transition_in > 0 {
                let d = clip.transition_in as f64 / fps as f64;
                chain.push_str(&format!(",afade=t=in:st=0:d={}", f(d)));
            }
            if tail > 0 {
                let d = tail as f64 / fps as f64;
                chain.push_str(&format!(",afade=t=out:st={}:d={}", f(played - d), f(d)));
            }
            if delay_ms > 0 {
                chain.push_str(&format!(",adelay={delay_ms}|{delay_ms}"));
            }
            chain.push_str(&format!("[{lbl}]"));
            g.push(chain);
            alabels.push(lbl);
        }
    }

    let has_audio = settings.include_audio && !alabels.is_empty();
    if has_audio {
        let joined: String = alabels.iter().map(|l| format!("[{l}]")).collect();
        g.push(format!(
            "{joined}amix=inputs={}:normalize=0:dropout_transition=0,\
             alimiter=limit=0.97,aresample=48000[aout]",
            alabels.len()
        ));
    }
    (inputs, g.join(), has_audio)
}

/// ffmpeg's `atempo` only accepts 0.5–2.0 per instance, so larger speed changes are chained.
fn atempo_chain(speed: f64) -> Vec<f64> {
    if (speed - 1.0).abs() < 1e-6 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut remaining = speed.clamp(0.05, 20.0);
    while remaining > 2.0 {
        out.push(2.0);
        remaining /= 2.0;
    }
    while remaining < 0.5 {
        out.push(0.5);
        remaining /= 0.5;
    }
    out.push(remaining);
    out
}

// ---------------------------------------------------------------------------
// Full-resolution pictures for the render graph
// ---------------------------------------------------------------------------

/// How far ahead it is worth decoding-and-discarding before it becomes cheaper to seek again.
const SKIP_LIMIT: i64 = 240;

/// One ffmpeg process per clip, decoding the original file at full resolution.
///
/// A render walks the timeline forwards, so each clip's source is read forwards too and one
/// sequential decoder per clip is all it takes. Two clips over the same file — a dissolve, or a
/// picture-in-picture of the same shot — get one decoder each, which is why the frame source is
/// keyed on the clip and not on the media.
struct ClipDec {
    child: std::process::Child,
    out: std::io::BufReader<std::process::ChildStdout>,
    w: u32,
    h: u32,
    /// Source frame index, at project rate, that the pipe will yield next.
    next: i64,
    last: Option<Arc<DecodedFrame>>,
    eof: bool,
}

impl Drop for ClipDec {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl ClipDec {
    fn open(ffmpeg: &std::path::Path, path: &std::path::Path, w: u32, h: u32, fps: u32, from: i64) -> Result<Self> {
        let mut cmd = ffmpeg::command(ffmpeg);
        cmd.args(["-hide_banner", "-v", "error", "-nostdin"]);
        if from > 0 {
            // Input-side seeking, which ffmpeg makes accurate by decoding forward from the
            // preceding keyframe. Output frame 0 is then the frame we asked for.
            cmd.args(["-ss", &format!("{:.6}", from as f64 / fps.max(1) as f64)]);
        }
        cmd.arg("-i").arg(path);
        // Resample to the project rate so a source frame index is a timeline frame index.
        cmd.args(["-an", "-sn", "-vf", &format!("fps={fps},format=rgba")]);
        cmd.args(["-f", "rawvideo", "-pix_fmt", "rgba", "-"]);
        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("starting a decoder for {}", path.display()))?;
        let out = std::io::BufReader::with_capacity(
            1 << 20,
            child.stdout.take().context("decoder produced no pipe")?,
        );
        Ok(Self { child, out, w, h, next: from, last: None, eof: false })
    }

    fn read_one(&mut self) -> Option<Arc<DecodedFrame>> {
        if self.eof {
            return None;
        }
        let mut buf = vec![0u8; (self.w as usize) * (self.h as usize) * 4];
        if self.out.read_exact(&mut buf).is_err() {
            self.eof = true;
            return None;
        }
        self.next += 1;
        let frame = Arc::new(DecodedFrame { width: self.w, height: self.h, rgba: buf });
        self.last = Some(frame.clone());
        Some(frame)
    }
}

/// The export's answer to "give me this clip's picture at this source frame".
pub struct ExportFrames {
    ffmpeg: PathBuf,
    fps: u32,
    /// path, width, height for every media item the timeline uses.
    media: BTreeMap<MediaId, (PathBuf, u32, u32)>,
    decoders: HashMap<ClipId, ClipDec>,
}

impl ExportFrames {
    pub fn new(tools: &Tools, project: &Project, fps: u32) -> Self {
        let mut media = BTreeMap::new();
        for m in &project.media {
            if m.has_video {
                media.insert(m.id, (m.path.clone(), m.src_width.max(1), m.src_height.max(1)));
            }
        }
        Self { ffmpeg: tools.ffmpeg.clone(), fps, media, decoders: HashMap::new() }
    }

    /// Clips that are behind us will never be asked for again; letting their decoders go keeps a
    /// long timeline from accumulating one ffmpeg process per cut.
    pub fn retain(&mut self, live: &[ClipId]) {
        self.decoders.retain(|k, _| live.contains(k));
    }
}

impl FrameSource for ExportFrames {
    fn frame(&mut self, clip: ClipId, media: MediaId, src_frame: i64) -> Option<Arc<DecodedFrame>> {
        let (path, w, h) = self.media.get(&media)?.clone();
        let src_frame = src_frame.max(0);
        let need_restart = match self.decoders.get(&clip) {
            None => true,
            // Behind the pipe, or so far ahead that seeking beats discarding.
            Some(d) => src_frame + 1 < d.next || src_frame > d.next + SKIP_LIMIT,
        };
        if need_restart {
            match ClipDec::open(&self.ffmpeg, &path, w, h, self.fps, src_frame) {
                Ok(d) => {
                    self.decoders.insert(clip, d);
                }
                Err(_) => return None,
            }
        }
        let d = self.decoders.get_mut(&clip)?;
        if src_frame + 1 == d.next {
            // Already sitting on it — a slowed-down clip asks for the same frame repeatedly.
            return d.last.clone();
        }
        while d.next <= src_frame {
            if d.read_one().is_none() {
                // Past the end of the source: hold the last frame rather than punch a hole.
                return d.last.clone();
            }
        }
        d.last.clone()
    }
}

// ---------------------------------------------------------------------------
// Running a render
// ---------------------------------------------------------------------------

pub fn start(
    tools: Arc<Tools>,
    project: Project,
    timeline: crate::project::TimelineId,
    settings: ExportSettings,
) -> ExportJob {
    let (tx, rx) = crossbeam_channel::unbounded();
    let cancel = Arc::new(AtomicBool::new(false));
    let c2 = cancel.clone();
    let s2 = settings.clone();

    std::thread::Builder::new()
        .name("kite-export".into())
        .spawn(move || {
            let Some(tl) = project.timeline_by_id(timeline).cloned() else {
                let _ = tx.send(ExportMsg::Failed("that timeline no longer exists".into()));
                return;
            };
            let res = run_export(&tools, &project, &tl, &s2, &tx, &c2);
            match res {
                Ok(()) => {
                    if c2.load(Ordering::Relaxed) {
                        let _ = tx.send(ExportMsg::Failed("Export cancelled".into()));
                    } else {
                        // Read the finished file back so what we report is what is really there.
                        let probed = ffmpeg::probe(&tools, &s2.path).ok();
                        let _ = tx.send(ExportMsg::Done {
                            path: s2.path.clone(),
                            width: probed.as_ref().map(|p| p.width).unwrap_or(s2.width),
                            height: probed.as_ref().map(|p| p.height).unwrap_or(s2.height),
                            duration: probed.as_ref().map(|p| p.duration).unwrap_or(0.0),
                            has_audio: probed.as_ref().map(|p| p.has_audio).unwrap_or(false),
                        });
                    }
                }
                Err(e) => {
                    let _ = tx.send(ExportMsg::Failed(format!("{e:#}")));
                }
            }
        })
        .expect("spawn export thread");

    ExportJob { rx, cancel, settings }
}

fn even(v: u32) -> u32 {
    if v < 2 {
        2
    } else {
        v - (v % 2)
    }
}

/// Which clips contribute to a frame, so decoders for clips already behind us can be closed.
fn live_clips(plan: &crate::render::FramePlan) -> Vec<ClipId> {
    plan.layers
        .iter()
        .filter_map(|l| match &l.source {
            crate::render::LayerSource::Media { clip, .. } => Some(*clip),
            _ => None,
        })
        .collect()
}

fn run_export(
    tools: &Tools,
    project: &Project,
    tl: &Timeline,
    settings: &ExportSettings,
    tx: &crossbeam_channel::Sender<ExportMsg>,
    cancel: &AtomicBool,
) -> Result<()> {
    let total_frames = tl.duration();
    if total_frames <= 0 {
        bail!("the timeline is empty — add a clip before exporting");
    }
    let w = even(settings.width);
    let h = even(settings.height);
    let fps = settings.fps.max(1);

    let mut renderer = Renderer::new(Gpu::headless()?)
        .context("preparing the render graph")?;
    let mut frames = ExportFrames::new(tools, project, fps);

    let (audio_inputs, audio_graph, has_audio) = build_audio_graph(project, tl, settings, 1);

    // The audio graph and any staging it needs live in their own directory, which is also
    // ffmpeg's working directory, so nothing on the command line has to be escaped.
    let staging = std::env::temp_dir().join(format!("kite-render-{}", std::process::id()));
    std::fs::create_dir_all(&staging).context("creating the render staging directory")?;
    let out_path = std::path::absolute(&settings.path).unwrap_or_else(|_| settings.path.clone());

    let mut cmd = ffmpeg::command(&tools.ffmpeg);
    cmd.current_dir(&staging);
    cmd.args(["-hide_banner", "-v", "error", "-y"]);
    // Input 0 is us.
    cmd.args(["-f", "rawvideo", "-pix_fmt", "rgba"]);
    cmd.args(["-s", &format!("{w}x{h}"), "-r", &fps.to_string(), "-i", "pipe:0"]);
    for i in &audio_inputs {
        cmd.arg("-i").arg(std::path::absolute(i).unwrap_or_else(|_| i.clone()));
    }
    if has_audio {
        // Even an audio-only graph can outgrow a Windows command line on a heavily cut edit.
        match graph_arg(tools) {
            GraphArg::SlashFile => {
                std::fs::write(staging.join("graph.txt"), audio_graph.as_bytes())
                    .context("writing the audio filtergraph")?;
                cmd.args(["-/filter_complex", "graph.txt"]);
            }
            GraphArg::ScriptFile => {
                std::fs::write(staging.join("graph.txt"), audio_graph.as_bytes())
                    .context("writing the audio filtergraph")?;
                cmd.args(["-filter_complex_script", "graph.txt"]);
            }
            GraphArg::Inline => {
                cmd.arg("-filter_complex").arg(&audio_graph);
            }
        }
    }
    cmd.args(["-map", "0:v"]);
    if has_audio {
        cmd.args(["-map", "[aout]"]);
    }

    let enc = settings.encoder;
    cmd.args(["-c:v", enc.name()]);
    match enc {
        Encoder::X264 => {
            cmd.args(["-preset", "veryfast", "-crf", &settings.quality.crf().to_string()]);
        }
        Encoder::NvencH264 => {
            cmd.args(["-preset", "p4", "-rc", "vbr", "-cq", &settings.quality.crf().to_string()]);
        }
        Encoder::QsvH264 => {
            cmd.args(["-global_quality", &settings.quality.crf().to_string()]);
        }
        Encoder::AmfH264 => {
            cmd.args(["-quality", "balanced", "-rc", "cqp", "-qp_i", &settings.quality.crf().to_string()]);
        }
    }
    cmd.args(["-pix_fmt", "yuv420p", "-r", &fps.to_string()]);
    // Interleave keyframes for streaming sites and put the index up front.
    cmd.args(["-g", &(fps * 2).to_string(), "-movflags", "+faststart"]);
    if has_audio {
        cmd.args(["-c:a", "aac", "-b:a", &format!("{}k", settings.quality.audio_kbps())]);
        // The mix is as long as the timeline; the picture decides when the file ends.
        cmd.args(["-shortest"]);
    }
    cmd.arg(&out_path);

    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting ffmpeg to encode the render")?;

    // ffmpeg's diagnostics have to be drained on another thread, or a full stderr pipe deadlocks
    // against us blocking on stdin.
    let mut err_pipe = child.stderr.take().expect("piped stderr");
    let errors = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err_pipe.read_to_string(&mut s);
        s
    });
    let mut sink = child.stdin.take().expect("piped stdin");

    let started = std::time::Instant::now();
    let mut written = 0i64;
    let mut render_fail: Option<anyhow::Error> = None;
    for frame in 0..total_frames {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let plan = plan_frame(tl, frame, w, h);
        frames.retain(&live_clips(&plan));
        if let Err(e) = renderer.render(&plan, &mut frames) {
            render_fail = Some(e);
            break;
        }
        let pixels = match renderer.read_rgba() {
            Ok(p) => p,
            Err(e) => {
                render_fail = Some(e);
                break;
            }
        };
        if sink.write_all(&pixels).is_err() {
            // ffmpeg went away; its stderr will say why.
            break;
        }
        written += 1;
        if written % 8 == 0 || written == total_frames {
            let secs = started.elapsed().as_secs_f32().max(0.001);
            let _ = tx.send(ExportMsg::Progress {
                pct: (written as f32 / total_frames as f32 * 100.0).clamp(0.0, 100.0),
                frames: written,
                speed: format!("{:.2}x", written as f32 / fps as f32 / secs),
            });
        }
    }
    drop(sink);

    let status = child.wait().context("waiting for ffmpeg")?;
    let err = errors.join().unwrap_or_default();
    std::fs::remove_dir_all(&staging).ok();

    if let Some(e) = render_fail {
        std::fs::remove_file(&out_path).ok();
        return Err(e.context("rendering a frame"));
    }
    if cancel.load(Ordering::Relaxed) {
        std::fs::remove_file(&out_path).ok();
        return Ok(());
    }
    if !status.success() {
        let lines: Vec<&str> = err.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        let detail = if lines.is_empty() {
            "ffmpeg exited with an error but said nothing".to_string()
        } else {
            // Keep the tail, which is where ffmpeg puts the reason, but enough of it to act on.
            lines[lines.len().saturating_sub(6)..].join("  |  ")
        };
        bail!("{detail}");
    }
    if written != total_frames {
        return Err(anyhow!(
            "rendered {written} of {total_frames} frames before the encoder stopped"
        ));
    }
    Ok(())
}
