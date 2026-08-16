//! Export: turns the timeline into a single ffmpeg filtergraph over the **original** media.
//!
//! Playback uses proxies, but delivery never does — the graph below reads the source files at full
//! resolution, so what you export is the real quality regardless of what you edited against.

use crate::ffmpeg::{self, Tools};
use crate::project::{ClipSource, MediaId, Project, TextAlign, TrackKind};
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
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
}

#[derive(Debug)]
pub enum ExportMsg {
    Progress { pct: f32, frames: i64, speed: String },
    Done(PathBuf),
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

/// Files the filtergraph needs to reference by name.
///
/// Quoting a Windows path inside an ffmpeg filtergraph is a well-known source of breakage: the
/// drive-letter colon is an option separator, and getting the escaping subtly wrong makes ffmpeg
/// either fail outright or quietly substitute a fallback font. Instead we stage the font and the
/// title text in one directory, run ffmpeg with that as its working directory, and refer to them
/// by bare filename. There is then nothing to escape.
pub struct Assets {
    pub dir: PathBuf,
    pub font_file: Option<String>,
    counter: std::cell::Cell<usize>,
}

impl Assets {
    pub fn prepare(font: Option<&Path>) -> Result<Self> {
        let dir = std::env::temp_dir().join(format!(
            "kite-render-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).context("creating the render staging directory")?;
        let font_file = match font {
            Some(src) => {
                let ext = src
                    .extension()
                    .map(|e| e.to_string_lossy().to_lowercase())
                    .unwrap_or_else(|| "ttf".into());
                let name = format!("font.{ext}");
                std::fs::copy(src, dir.join(&name))
                    .with_context(|| format!("copying the title font from {}", src.display()))?;
                Some(name)
            }
            None => None,
        };
        Ok(Self { dir, font_file, counter: std::cell::Cell::new(0) })
    }

    /// Writes title text to its own file so the text itself never needs escaping either —
    /// quotes, colons, percent signs, newlines and emoji all pass through untouched.
    fn write_text(&self, text: &str) -> Result<String> {
        let n = self.counter.get();
        self.counter.set(n + 1);
        let name = format!("text{n}.txt");
        std::fs::write(self.dir.join(&name), text.as_bytes())
            .context("writing title text for the renderer")?;
        Ok(name)
    }

    pub fn cleanup(&self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

fn f(t: f64) -> String {
    format!("{t:.6}")
}

fn even(v: i64) -> i64 {
    if v < 2 {
        2
    } else {
        v - (v % 2)
    }
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

/// Builds the complete `-filter_complex` string plus the ordered list of input files.
pub fn build_graph(
    project: &Project,
    settings: &ExportSettings,
    assets: Option<&Assets>,
) -> Result<(Vec<PathBuf>, String, bool)> {
    let w = settings.width as i64;
    let h = settings.height as i64;
    let fps = settings.fps.max(1);
    let total_frames = project.duration();
    if total_frames <= 0 {
        bail!("the timeline is empty — add a clip before exporting");
    }
    let dur = total_frames as f64 / fps as f64;

    // One ffmpeg input per distinct source file, reused by every clip that references it.
    let mut inputs: Vec<PathBuf> = Vec::new();
    let mut index_of: BTreeMap<MediaId, usize> = BTreeMap::new();
    for track in &project.tracks {
        for clip in &track.clips {
            if let Some(mid) = clip.media_id() {
                if !index_of.contains_key(&mid) {
                    let Some(m) = project.media(mid) else { continue };
                    index_of.insert(mid, inputs.len());
                    inputs.push(m.path.clone());
                }
            }
        }
    }

    let mut g = Graph::new();
    g.push(format!(
        "color=c=black:s={w}x{h}:r={fps}:d={}[bg]",
        f(dur)
    ));
    let mut current = "bg".to_string();

    // Video tracks composite bottom-up, so walk them in reverse (they are stored top-first).
    let video_tracks: Vec<_> = project
        .tracks
        .iter()
        .filter(|t| t.kind == TrackKind::Video && !t.hidden)
        .collect();

    for track in video_tracks.iter().rev() {
        for (i, clip) in track.clips.iter().enumerate() {
            // If the next clip dissolves in, this one keeps rolling underneath for that long.
            let tail = track
                .clips
                .get(i + 1)
                .map(|n| n.transition_in.max(0))
                .unwrap_or(0);
            let t0 = clip.start as f64 / fps as f64;
            let t1 = (clip.end() + tail) as f64 / fps as f64;
            let fade_in = clip.fade_in as f64 / fps as f64;
            let fade_out = clip.fade_out as f64 / fps as f64;
            let dissolve_in = clip.transition_in.max(0) as f64 / fps as f64;
            let dissolve_out = tail as f64 / fps as f64;

            match &clip.source {
                ClipSource::Media(mid) => {
                    let Some(m) = project.media(*mid) else { continue };
                    if !m.has_video {
                        continue;
                    }
                    let Some(&idx) = index_of.get(mid) else { continue };
                    let speed = clip.speed.max(0.01) as f64;
                    let src_in = clip.src_in as f64 / fps as f64;
                    let src_out = src_in + ((clip.len + tail) as f64 * speed / fps as f64);

                    let (cw, ch) = fitted_size(m.src_width, m.src_height, w, h, clip.scale);
                    let lbl = g.label("v");
                    let mut chain = format!("[{idx}:v]trim=start={}:end={}", f(src_in), f(src_out));
                    // setpts both retimes for speed and slides the clip to its place on the line.
                    if (speed - 1.0).abs() > 1e-6 {
                        chain.push_str(&format!(",setpts=(PTS-STARTPTS)/{}+{}/TB", f(speed), f(t0)));
                    } else {
                        chain.push_str(&format!(",setpts=PTS-STARTPTS+{}/TB", f(t0)));
                    }
                    if !clip.color.is_neutral() {
                        chain.push_str(&format!(
                            ",eq=contrast={:.4}:brightness={:.4}:saturation={:.4}",
                            clip.color.contrast, clip.color.brightness, clip.color.saturation
                        ));
                    }
                    chain.push_str(&format!(
                        ",scale={cw}:{ch}:flags=bicubic,fps={fps},format=rgba"
                    ));
                    if clip.opacity < 0.999 {
                        chain.push_str(&format!(",colorchannelmixer=aa={:.4}", clip.opacity));
                    }
                    if fade_in > 0.0 {
                        chain.push_str(&format!(
                            ",fade=t=in:st={}:d={}:alpha=1",
                            f(t0),
                            f(fade_in)
                        ));
                    }
                    if fade_out > 0.0 {
                        chain.push_str(&format!(
                            ",fade=t=out:st={}:d={}:alpha=1",
                            f(t1 - dissolve_out - fade_out),
                            f(fade_out)
                        ));
                    }
                    // The dissolve itself: this clip fades away while the next fades up over it.
                    if dissolve_in > 0.0 {
                        chain.push_str(&format!(
                            ",fade=t=in:st={}:d={}:alpha=1",
                            f(t0),
                            f(dissolve_in)
                        ));
                    }
                    if dissolve_out > 0.0 {
                        chain.push_str(&format!(
                            ",fade=t=out:st={}:d={}:alpha=1",
                            f(t1 - dissolve_out),
                            f(dissolve_out)
                        ));
                    }
                    chain.push_str(&format!("[{lbl}]"));
                    g.push(chain);

                    let (x, y) = position(w, h, cw, ch, clip.pos_x, clip.pos_y);
                    let out = g.label("c");
                    g.push(format!(
                        "[{current}][{lbl}]overlay=x={x}:y={y}:eof_action=pass:\
                         enable='between(t,{},{})'[{out}]",
                        f(t0),
                        f(t1)
                    ));
                    current = out;
                }
                ClipSource::Color(rgba) => {
                    let lbl = g.label("v");
                    let mut chain = format!(
                        "color=c=0x{:02x}{:02x}{:02x}@{:.3}:s={w}x{h}:r={fps}:d={},format=rgba,\
                         setpts=PTS-STARTPTS+{}/TB",
                        rgba[0],
                        rgba[1],
                        rgba[2],
                        rgba[3] as f32 / 255.0,
                        f(t1 - t0),
                        f(t0)
                    );
                    if dissolve_in > 0.0 {
                        chain.push_str(&format!(
                            ",fade=t=in:st={}:d={}:alpha=1",
                            f(t0),
                            f(dissolve_in)
                        ));
                    }
                    if dissolve_out > 0.0 {
                        chain.push_str(&format!(
                            ",fade=t=out:st={}:d={}:alpha=1",
                            f(t1 - dissolve_out),
                            f(dissolve_out)
                        ));
                    }
                    chain.push_str(&format!("[{lbl}]"));
                    g.push(chain);
                    let out = g.label("c");
                    g.push(format!(
                        "[{current}][{lbl}]overlay=x=0:y=0:eof_action=pass:\
                         enable='between(t,{},{})'[{out}]",
                        f(t0),
                        f(t1)
                    ));
                    current = out;
                }
                ClipSource::Text(tp) => {
                    // Without a usable font we skip titles rather than emit a graph that fails.
                    let Some(assets) = assets else { continue };
                    let Some(fontname) = assets.font_file.as_deref() else { continue };
                    let textfile = assets.write_text(&tp.text)?;
                    let size = (tp.size * h as f32).round().max(8.0) as i64;
                    let x_expr = match tp.align {
                        TextAlign::Left => format!("{}", (tp.x * w as f32).round() as i64),
                        TextAlign::Center => {
                            format!("{}-text_w/2", (tp.x * w as f32).round() as i64)
                        }
                        TextAlign::Right => {
                            format!("{}-text_w", (tp.x * w as f32).round() as i64)
                        }
                    };
                    let y = (tp.y * h as f32).round() as i64 - size / 2;
                    let out = g.label("c");
                    let mut d = format!(
                        "[{current}]drawtext=fontfile={fontname}:textfile={textfile}\
                         :expansion=none:fontsize={size}\
                         :fontcolor=0x{:02x}{:02x}{:02x}@{:.3}:x={x_expr}:y={y}",
                        tp.color[0],
                        tp.color[1],
                        tp.color[2],
                        tp.color[3] as f32 / 255.0,
                    );
                    if tp.shadow {
                        d.push_str(":shadowcolor=black@0.6:shadowx=2:shadowy=2");
                    }
                    if tp.box_bg {
                        d.push_str(":box=1:boxcolor=black@0.5:boxborderw=12");
                    }
                    d.push_str(&format!(":enable='between(t,{},{})'[{out}]", f(t0), f(t1)));
                    g.push(d);
                    current = out;
                }
            }
        }
    }

    g.push(format!("[{current}]format=yuv420p[vout]"));

    // --- audio ---
    let mut alabels: Vec<String> = Vec::new();
    for track in project.tracks.iter().filter(|t| !t.muted) {
        for (i, clip) in track.clips.iter().enumerate() {
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
            // A muted video track still contributes its audio unless the track itself is muted.
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

    let has_audio = !alabels.is_empty();
    if has_audio {
        let joined: String = alabels.iter().map(|l| format!("[{l}]")).collect();
        g.push(format!(
            "{joined}amix=inputs={}:normalize=0:dropout_transition=0,\
             alimiter=limit=0.97,aresample=48000[aout]",
            alabels.len()
        ));
    }

    Ok((inputs, g.join(), has_audio))
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

fn fitted_size(sw: u32, sh: u32, w: i64, h: i64, scale: f32) -> (i64, i64) {
    let sw = sw.max(1) as f64;
    let sh = sh.max(1) as f64;
    let fit = (w as f64 / sw).min(h as f64 / sh) * scale.max(0.01) as f64;
    (even((sw * fit).round() as i64), even((sh * fit).round() as i64))
}

fn position(w: i64, h: i64, cw: i64, ch: i64, px: f32, py: f32) -> (i64, i64) {
    let x = (w - cw) / 2 + (px * w as f32).round() as i64;
    let y = (h - ch) / 2 + (py * h as f32).round() as i64;
    (x, y)
}

pub fn start(
    tools: Arc<Tools>,
    project: Project,
    settings: ExportSettings,
    font: Option<PathBuf>,
) -> ExportJob {
    let (tx, rx) = crossbeam_channel::unbounded();
    let cancel = Arc::new(AtomicBool::new(false));
    let c2 = cancel.clone();
    let s2 = settings.clone();

    std::thread::Builder::new()
        .name("kite-export".into())
        .spawn(move || {
            let total_frames = project.duration();
            let assets = match Assets::prepare(font.as_deref()) {
                Ok(a) => a,
                Err(e) => {
                    let _ = tx.send(ExportMsg::Failed(format!("{e:#}")));
                    return;
                }
            };
            let res = run_export(&tools, &project, &s2, &assets, total_frames, &tx, &c2);
            assets.cleanup();
            match res {
                Ok(()) => {
                    if c2.load(Ordering::Relaxed) {
                        let _ = tx.send(ExportMsg::Failed("Export cancelled".into()));
                    } else {
                        let _ = tx.send(ExportMsg::Done(s2.path.clone()));
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

fn run_export(
    tools: &Tools,
    project: &Project,
    settings: &ExportSettings,
    assets: &Assets,
    total_frames: i64,
    tx: &crossbeam_channel::Sender<ExportMsg>,
    cancel: &AtomicBool,
) -> Result<()> {
    let (inputs, graph, has_audio) = build_graph(project, settings, Some(assets))?;

    let mut cmd = ffmpeg::command(&tools.ffmpeg);
    // Everything the filtergraph names is in here, referenced without a path.
    cmd.current_dir(&assets.dir);
    cmd.args(["-hide_banner", "-v", "error", "-nostdin", "-y"]);
    for i in &inputs {
        cmd.arg("-i").arg(i);
    }
    // A real edit produces a graph far larger than a Windows command line allows, so it goes to
    // ffmpeg in a file whenever this build supports it.
    match graph_arg(tools) {
        GraphArg::SlashFile => {
            std::fs::write(assets.dir.join("graph.txt"), graph.as_bytes())
                .context("writing the filtergraph")?;
            cmd.args(["-/filter_complex", "graph.txt"]);
        }
        GraphArg::ScriptFile => {
            std::fs::write(assets.dir.join("graph.txt"), graph.as_bytes())
                .context("writing the filtergraph")?;
            cmd.args(["-filter_complex_script", "graph.txt"]);
        }
        GraphArg::Inline => {
            cmd.arg("-filter_complex").arg(&graph);
        }
    }
    cmd.args(["-map", "[vout]"]);
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
    cmd.args(["-pix_fmt", "yuv420p", "-r", &settings.fps.to_string()]);
    // Interleave keyframes for streaming sites and put the index up front.
    cmd.args(["-g", &(settings.fps * 2).to_string(), "-movflags", "+faststart"]);
    if has_audio {
        cmd.args(["-c:a", "aac", "-b:a", &format!("{}k", settings.quality.audio_kbps())]);
    }
    cmd.args(["-progress", "pipe:1", "-nostats"]);
    cmd.arg(&settings.path);

    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting ffmpeg for export")?;

    let stdout = child.stdout.take().expect("piped stdout");
    let reader = BufReader::new(stdout);
    let mut frames = 0i64;
    let mut speed = String::new();

    for line in reader.lines() {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            std::fs::remove_file(&settings.path).ok();
            return Ok(());
        }
        let Ok(line) = line else { break };
        if let Some(v) = line.strip_prefix("frame=") {
            frames = v.trim().parse().unwrap_or(frames);
        } else if let Some(v) = line.strip_prefix("speed=") {
            speed = v.trim().to_string();
        } else if line.starts_with("progress=") {
            let pct = if total_frames > 0 {
                (frames as f32 / total_frames as f32 * 100.0).clamp(0.0, 100.0)
            } else {
                0.0
            };
            let _ = tx.send(ExportMsg::Progress { pct, frames, speed: speed.clone() });
        }
    }

    let status = child.wait().context("waiting for ffmpeg")?;
    if !status.success() {
        let mut err = String::new();
        use std::io::Read;
        if let Some(mut e) = child.stderr.take() {
            e.read_to_string(&mut err).ok();
        }
        let lines: Vec<&str> = err.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        let detail = if lines.is_empty() {
            "ffmpeg exited with an error but said nothing".to_string()
        } else {
            // Keep the tail, which is where ffmpeg puts the reason, but enough of it to act on.
            lines[lines.len().saturating_sub(6)..].join("  |  ")
        };
        bail!("{detail}");
    }
    Ok(())
}
