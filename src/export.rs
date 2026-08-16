//! Export: renders the timeline through the GPU graph, frame by frame, and hands raw pixels to
//! ffmpeg to encode.
//!
//! Playback uses proxies, but delivery never does — the decoders below read the source files at
//! full resolution, so what you export is the real quality regardless of what you edited against.
//!
//! Compositing is **not** here, and neither is mixing. Pictures come from `render.rs` and sound
//! from `mix.rs`, both shared with the preview. That is the whole point of the arrangement:
//! ffmpeg does demux, decode and encode, and nothing else. There is no filtergraph any more.

use crate::decode::DecodedFrame;
use crate::ffmpeg::{self, Tools};
use crate::mix::{plan_audio, AudioPlan, PcmSource, RetimeCache, WavWriter};
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
// Sound for the mixer
// ---------------------------------------------------------------------------

/// Supplies the mixer with 48 kHz stereo samples for every media item the timeline uses.
///
/// The importer already writes exactly this file for anything brought into a project, so the
/// usual case is a memory map of the same bytes the preview is mixing from — which is why the two
/// paths can be compared sample by sample at all. A project rendered from the command line, or
/// one whose cache has been cleared, may not have it, so anything missing is extracted here with
/// the same ffmpeg invocation the importer uses.
pub struct ExportPcm {
    ffmpeg: PathBuf,
    dir: PathBuf,
    /// Source file and the importer's extracted PCM, if it wrote one.
    media: BTreeMap<MediaId, (PathBuf, Option<PathBuf>)>,
    opened: HashMap<MediaId, Option<Arc<memmap2::Mmap>>>,
}

impl ExportPcm {
    pub fn new(tools: &Tools, project: &Project, dir: PathBuf) -> Self {
        let mut media = BTreeMap::new();
        for m in &project.media {
            if m.has_audio {
                media.insert(m.id, (m.path.clone(), m.audio_path.clone()));
            }
        }
        Self { ffmpeg: tools.ffmpeg.clone(), dir, media, opened: HashMap::new() }
    }

    fn extract(&self, media: MediaId, src: &std::path::Path) -> Result<PathBuf> {
        let out = self.dir.join(format!("audio{media}.pcm"));
        let file = std::fs::File::create(&out)
            .with_context(|| format!("creating {}", out.display()))?;
        let status = ffmpeg::command(&self.ffmpeg)
            .args(["-v", "error", "-nostdin", "-i"])
            .arg(src)
            .args(["-vn", "-sn", "-dn", "-ac", "2", "-ar"])
            .arg(crate::project::SAMPLE_RATE.to_string())
            .args(["-f", "s16le", "-acodec", "pcm_s16le", "-"])
            .stdout(Stdio::from(file))
            .stderr(Stdio::null())
            .status()
            .with_context(|| format!("extracting audio from {}", src.display()))?;
        if !status.success() {
            bail!("ffmpeg could not read the audio of {}", src.display());
        }
        Ok(out)
    }
}

impl PcmSource for ExportPcm {
    fn pcm(&mut self, media: MediaId) -> Option<Arc<memmap2::Mmap>> {
        if let Some(v) = self.opened.get(&media) {
            return v.clone();
        }
        let (src, cached) = self.media.get(&media)?.clone();
        let path = match cached.filter(|p| p.is_file()) {
            Some(p) => Some(p),
            None => self.extract(media, &src).ok(),
        };
        let mapped = path.and_then(|p| crate::audio::open_pcm(&p));
        self.opened.insert(media, mapped.clone());
        mapped
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

    // Everything the render needs to stage lives in its own directory.
    let staging = std::env::temp_dir().join(format!(
        "kite-render-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&staging).context("creating the render staging directory")?;
    let out_path = std::path::absolute(&settings.path).unwrap_or_else(|_| settings.path.clone());

    // Sound is mixed first, whole, to a float WAV. Two live pipes into one ffmpeg — raw video on
    // one and raw audio on the other — is a deadlock waiting to happen, and the mix is small
    // next to the pictures, so this is the cheap way out rather than a compromise.
    let (wav, plan) = if settings.include_audio {
        let mut pcm = ExportPcm::new(tools, project, staging.clone());
        let plan = plan_audio(project, tl, &mut pcm, &mut RetimeCache::default());
        if plan.is_silent() {
            (None, plan)
        } else {
            let path = staging.join("mix.wav");
            write_mix(&plan, &path, total_frames, settings.fps)?;
            (Some(path), plan)
        }
    } else {
        (None, AudioPlan::default())
    };
    let has_audio = wav.is_some();

    let mut cmd = ffmpeg::command(&tools.ffmpeg);
    cmd.current_dir(&staging);
    cmd.args(["-hide_banner", "-v", "error", "-y"]);
    // Input 0 is us.
    cmd.args(["-f", "rawvideo", "-pix_fmt", "rgba"]);
    cmd.args(["-s", &format!("{w}x{h}"), "-r", &fps.to_string(), "-i", "pipe:0"]);
    if let Some(w) = &wav {
        cmd.arg("-i").arg(w);
    }
    cmd.args(["-map", "0:v"]);
    if has_audio {
        cmd.args(["-map", "1:a"]);
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
        // The mix is exactly as long as the timeline; the picture decides when the file ends.
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

    // A timeline that should have made a noise and a file that did not is the failure a tester
    // reported and nobody could reproduce. It is no longer allowed to pass quietly.
    if has_audio {
        let probed = ffmpeg::probe(tools, &out_path)
            .context("reading back the finished file to check its sound")?;
        check_audio_arrived(plan.layers.len(), probed.has_audio)?;
    }
    Ok(())
}

/// The last word on whether a render kept its sound.
///
/// Split out so it can be tested: a silent file from a timeline that had audio clips on it is the
/// exact shape of the bug a tester reported and nobody could reproduce, and "the export succeeded"
/// must never again be the whole story.
fn check_audio_arrived(layers: usize, file_has_audio: bool) -> Result<()> {
    if layers > 0 && !file_has_audio {
        bail!(
            "the timeline has {layers} audio clip(s) and a mix was written for them, but the \
             rendered file has no sound track — the encoder dropped it"
        );
    }
    Ok(())
}

/// Mixes the whole timeline to a float WAV.
///
/// The mix is generated by exactly the code the preview plays through, so what lands here is what
/// was being monitored — not a second implementation that has to be kept in step by hand.
fn write_mix(
    plan: &AudioPlan,
    path: &std::path::Path,
    total_frames: i64,
    fps: u32,
) -> Result<()> {
    // Match the picture exactly rather than trusting the plan's own idea of the length.
    let total = (total_frames as i128 * crate::project::SAMPLE_RATE as i128
        / fps.max(1) as i128) as i64;
    let mut wav = WavWriter::create(path).context("creating the mixed audio file")?;
    let mut scratch = Vec::new();
    let mut block = vec![0f32; 8192 * 2];
    let mut at = 0i64;
    while at < total {
        let n = ((total - at) as usize).min(8192);
        plan.mix_into(at, &mut block[..n * 2], &mut scratch);
        wav.write(&block[..n * 2]).context("writing the mixed audio")?;
        at += n as i64;
    }
    let frames = wav.finish().context("finishing the mixed audio file")?;
    if frames as i64 != total {
        bail!("the mix came out {frames} samples long, expected {total}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_render_that_loses_its_sound_is_an_error() {
        assert!(check_audio_arrived(3, false).is_err(), "silent output must not pass");
        assert!(check_audio_arrived(3, true).is_ok());
        // Nothing to lose is not a failure — a timeline of colour cards is legitimately silent.
        assert!(check_audio_arrived(0, false).is_ok());
    }
}
