//! Background import: probe, build the proxy frame store, extract audio, compute peaks.
//!
//! Nothing here ever runs on the UI thread. Results are content-addressed by source path, size,
//! mtime and proxy parameters, so re-importing a file you have used before is instant and the
//! cache survives restarts.

use crate::ffmpeg::{self, ProbeInfo, Tools};
use crate::framestore::{scan_frames, FrameStoreWriter};
use crate::project::{MediaId, VideoSettings, SAMPLE_RATE};
use anyhow::{bail, Context, Result};
use crossbeam_channel::{Receiver, Sender};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

/// Samples per waveform bucket. ~94 buckets a second is plenty to draw from at any zoom.
pub const PEAK_BUCKET: usize = 512;

#[derive(Debug)]
pub enum ImportMsg {
    Probed { id: MediaId, info: ProbeInfo, frames: i64 },
    Progress { id: MediaId, pct: u8 },
    Ready { id: MediaId, proxy: Option<PathBuf>, audio: Option<PathBuf>, peaks: Option<PathBuf>, frames: i64 },
    Failed { id: MediaId, error: String },
}

struct Job {
    id: MediaId,
    path: PathBuf,
    settings: VideoSettings,
    proxy_height: u32,
}

pub struct Importer {
    tx_job: Sender<Job>,
    pub rx: Receiver<ImportMsg>,
    pub cache_dir: PathBuf,
}

impl Importer {
    pub fn new(tools: Arc<Tools>, cache_dir: PathBuf, workers: usize) -> Self {
        let (tx_job, rx_job) = crossbeam_channel::unbounded::<Job>();
        let (tx_msg, rx) = crossbeam_channel::unbounded::<ImportMsg>();
        for _ in 0..workers.max(1) {
            let rx_job = rx_job.clone();
            let tx_msg = tx_msg.clone();
            let tools = tools.clone();
            let cache = cache_dir.clone();
            std::thread::Builder::new()
                .name("kite-import".into())
                .spawn(move || {
                    while let Ok(job) = rx_job.recv() {
                        let id = job.id;
                        if let Err(e) = run_job(&tools, &cache, &job, &tx_msg) {
                            let _ = tx_msg.send(ImportMsg::Failed { id, error: format!("{e:#}") });
                        }
                    }
                })
                .expect("spawn import worker");
        }
        Self { tx_job, rx, cache_dir }
    }

    pub fn submit(&self, id: MediaId, path: PathBuf, settings: VideoSettings, proxy_height: u32) {
        let _ = self.tx_job.send(Job { id, path, settings, proxy_height });
    }
}

fn run_job(tools: &Tools, cache: &Path, job: &Job, tx: &Sender<ImportMsg>) -> Result<()> {
    let info = ffmpeg::probe(tools, &job.path)?;
    let fps = job.settings.fps;
    let frames = if info.duration > 0.0 {
        (info.duration * fps as f64).round() as i64
    } else {
        0
    };
    let _ = tx.send(ImportMsg::Probed { id: job.id, info: info.clone(), frames });

    let key = cache_key(&job.path, fps, job.proxy_height)?;
    let dir = cache.join(key);
    std::fs::create_dir_all(&dir).context("creating media cache directory")?;

    let proxy_path = dir.join("proxy.kfs");
    let audio_path = dir.join("audio.pcm");
    let peaks_path = dir.join("peaks.bin");
    let done_marker = dir.join("complete");

    // A previous session already built these; nothing to do.
    if done_marker.is_file() {
        let _ = tx.send(ImportMsg::Ready {
            id: job.id,
            proxy: info.has_video.then(|| proxy_path.clone()),
            audio: info.has_audio.then(|| audio_path.clone()),
            peaks: info.has_audio.then(|| peaks_path.clone()),
            frames: read_frame_count(&proxy_path).unwrap_or(frames),
        });
        return Ok(());
    }

    let mut real_frames = frames;

    if info.has_video {
        real_frames = build_proxy(tools, job, &proxy_path, frames, tx)?;
    }
    if info.has_audio {
        extract_audio(tools, job, &audio_path, &peaks_path)?;
        if !info.has_video {
            // Audio-only media: length comes from the sample count.
            let bytes = std::fs::metadata(&audio_path).map(|m| m.len()).unwrap_or(0);
            let samples = bytes / 4; // stereo i16
            real_frames = (samples as i128 * fps as i128 / SAMPLE_RATE as i128) as i64;
        }
    }

    std::fs::write(&done_marker, b"1").ok();
    let _ = tx.send(ImportMsg::Ready {
        id: job.id,
        proxy: info.has_video.then_some(proxy_path),
        audio: info.has_audio.then_some(audio_path),
        peaks: info.has_audio.then_some(peaks_path),
        frames: real_frames.max(1),
    });
    Ok(())
}

fn read_frame_count(proxy: &Path) -> Option<i64> {
    crate::framestore::FrameStore::open(proxy).ok().map(|s| s.frames as i64)
}

/// Transcodes to our indexed all-intra store, at the project frame rate so timeline frames and
/// source frames are the same integer.
fn build_proxy(
    tools: &Tools,
    job: &Job,
    out_path: &Path,
    expected: i64,
    tx: &Sender<ImportMsg>,
) -> Result<i64> {
    let scale = format!(
        "scale=-2:{}:flags=fast_bilinear,fps={}",
        job.proxy_height, job.settings.fps
    );
    let mut child = ffmpeg::command(&tools.ffmpeg)
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(&job.path)
        .args(["-an", "-sn", "-dn", "-vf", &scale])
        .args(["-c:v", "mjpeg", "-q:v", "6", "-pix_fmt", "yuvj420p"])
        .args(["-f", "image2pipe", "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting ffmpeg for proxy generation")?;

    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut writer = FrameStoreWriter::create(out_path, job.settings.fps)?;

    let mut buf: Vec<u8> = Vec::with_capacity(1 << 20);
    let mut chunk = vec![0u8; 256 * 1024];
    let mut count: i64 = 0;
    let mut last_pct = u8::MAX;

    loop {
        let n = stdout.read(&mut chunk).context("reading ffmpeg output")?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);

        let (ranges, consumed) = scan_frames(&buf);
        for r in ranges {
            writer.push(&buf[r])?;
            count += 1;
        }
        buf.drain(..consumed);

        if expected > 0 {
            let pct = ((count * 100 / expected.max(1)).clamp(0, 99)) as u8;
            if pct != last_pct {
                last_pct = pct;
                let _ = tx.send(ImportMsg::Progress { id: job.id, pct });
            }
        }
    }

    let status = child.wait().context("waiting for ffmpeg")?;
    let mut err = String::new();
    if let Some(mut e) = child.stderr.take() {
        e.read_to_string(&mut err).ok();
    }
    if !status.success() && count == 0 {
        bail!(
            "ffmpeg failed to decode this file: {}",
            err.lines().next().unwrap_or("unknown error")
        );
    }
    writer.finish()?;
    if count == 0 {
        bail!("no video frames were produced");
    }
    Ok(count)
}

/// Extracts interleaved stereo i16 at 48 kHz and computes the waveform envelope in the same pass.
fn extract_audio(tools: &Tools, job: &Job, out_path: &Path, peaks_path: &Path) -> Result<()> {
    let mut child = ffmpeg::command(&tools.ffmpeg)
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(&job.path)
        .args(["-vn", "-sn", "-dn", "-ac", "2", "-ar"])
        .arg(SAMPLE_RATE.to_string())
        .args(["-f", "s16le", "-acodec", "pcm_s16le", "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting ffmpeg for audio extraction")?;

    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut out = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(out_path)?);
    let mut peaks = std::io::BufWriter::with_capacity(1 << 16, std::fs::File::create(peaks_path)?);

    let mut chunk = vec![0u8; 256 * 1024];
    let mut carry: Vec<u8> = Vec::new();
    let mut bucket_min = i16::MAX;
    let mut bucket_max = i16::MIN;
    let mut in_bucket = 0usize;

    loop {
        let n = stdout.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        out.write_all(&chunk[..n])?;

        carry.extend_from_slice(&chunk[..n]);
        let usable = carry.len() - (carry.len() % 4); // whole stereo frames only
        for s in carry[..usable].chunks_exact(4) {
            let l = i16::from_le_bytes([s[0], s[1]]);
            let r = i16::from_le_bytes([s[2], s[3]]);
            let m = ((l as i32 + r as i32) / 2) as i16;
            bucket_min = bucket_min.min(m);
            bucket_max = bucket_max.max(m);
            in_bucket += 1;
            if in_bucket >= PEAK_BUCKET {
                peaks.write_all(&bucket_min.to_le_bytes())?;
                peaks.write_all(&bucket_max.to_le_bytes())?;
                bucket_min = i16::MAX;
                bucket_max = i16::MIN;
                in_bucket = 0;
            }
        }
        carry.drain(..usable);
    }
    if in_bucket > 0 {
        peaks.write_all(&bucket_min.to_le_bytes())?;
        peaks.write_all(&bucket_max.to_le_bytes())?;
    }
    out.flush()?;
    peaks.flush()?;

    let status = child.wait()?;
    if !status.success() {
        let mut err = String::new();
        if let Some(mut e) = child.stderr.take() {
            e.read_to_string(&mut err).ok();
        }
        bail!(
            "ffmpeg failed to extract audio: {}",
            err.lines().next().unwrap_or("unknown error")
        );
    }
    Ok(())
}

/// Identity of a derived artifact: same file, same size, same mtime, same proxy settings.
fn cache_key(path: &Path, fps: u32, height: u32) -> Result<String> {
    let md = std::fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut h = Fnv::new();
    h.write(path.to_string_lossy().as_bytes());
    h.write(&md.len().to_le_bytes());
    h.write(&mtime.to_le_bytes());
    h.write(&fps.to_le_bytes());
    h.write(&height.to_le_bytes());
    Ok(format!("{:016x}", h.finish()))
}

struct Fnv(u64);
impl Fnv {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= *b as u64;
            self.0 = self.0.wrapping_mul(0x1000_0000_01b3);
        }
    }
    fn finish(&self) -> u64 {
        self.0
    }
}
