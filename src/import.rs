//! Import: probe the file, then extract audio in the background.
//!
//! Video is deliberately *not* transcoded here. Playback spans are built on demand by
//! [`crate::proxy`] as the timeline asks for them, so a clip is editable the moment it has been
//! probed — a second or so — regardless of how long the recording is.

use crate::ffmpeg::{self, ProbeInfo, Tools};
use crate::project::{MediaId, SAMPLE_RATE};
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
    /// The file is understood and can be edited with straight away.
    Probed {
        id: MediaId,
        info: ProbeInfo,
        frames: i64,
        /// Where playback spans for this item at the current settings belong.
        video_dir: PathBuf,
    },
    /// Sound is now available for the waveform and for playback.
    AudioReady { id: MediaId, audio: PathBuf, peaks: PathBuf },
    Failed { id: MediaId, error: String },
}

struct Job {
    id: MediaId,
    path: PathBuf,
    fps: u32,
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

    pub fn submit(&self, id: MediaId, path: PathBuf, fps: u32, proxy_height: u32) {
        let _ = self.tx_job.send(Job { id, path, fps, proxy_height });
    }

    /// Where playback spans live for a given file and set of playback settings.
    pub fn video_dir(&self, path: &Path, fps: u32, height: u32) -> Result<PathBuf> {
        Ok(self.cache_dir.join(video_key(path, fps, height)?))
    }
}

fn run_job(tools: &Tools, cache: &Path, job: &Job, tx: &Sender<ImportMsg>) -> Result<()> {
    let info = ffmpeg::probe(tools, &job.path)?;
    let frames = if info.duration > 0.0 {
        (info.duration * job.fps as f64).round() as i64
    } else {
        0
    };

    let video_dir = cache.join(video_key(&job.path, job.fps, job.proxy_height)?);
    std::fs::create_dir_all(&video_dir).ok();
    let _ = tx.send(ImportMsg::Probed {
        id: job.id,
        info: info.clone(),
        frames: frames.max(1),
        video_dir,
    });

    if info.has_audio {
        let dir = cache.join(audio_key(&job.path)?);
        std::fs::create_dir_all(&dir).context("creating the media cache directory")?;
        let audio = dir.join("audio.pcm");
        let peaks = dir.join("peaks.bin");
        let done = dir.join("audio.complete");
        if !done.is_file() {
            extract_audio(tools, &job.path, &audio, &peaks)?;
            std::fs::write(&done, b"1").ok();
        }
        let _ = tx.send(ImportMsg::AudioReady { id: job.id, audio, peaks });
    }
    Ok(())
}

/// Extracts interleaved stereo i16 at 48 kHz and computes the waveform envelope in the same pass.
fn extract_audio(tools: &Tools, src: &Path, out_path: &Path, peaks_path: &Path) -> Result<()> {
    let mut child = ffmpeg::command(&tools.ffmpeg)
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(src)
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
        let usable = carry.len() - (carry.len() % 4);
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

fn file_identity(path: &Path) -> Result<(u64, u64)> {
    let md = std::fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok((md.len(), mtime))
}

/// Audio does not depend on playback resolution or project frame rate, so it is keyed separately
/// and survives changing either.
fn audio_key(path: &Path) -> Result<String> {
    let (len, mtime) = file_identity(path)?;
    let mut h = Fnv::new();
    h.write(path.to_string_lossy().as_bytes());
    h.write(&len.to_le_bytes());
    h.write(&mtime.to_le_bytes());
    h.write(b"audio");
    Ok(format!("a{:016x}", h.finish()))
}

/// Playback spans do depend on both, so changing either simply selects a different directory and
/// the spans rebuild on demand.
fn video_key(path: &Path, fps: u32, height: u32) -> Result<String> {
    let (len, mtime) = file_identity(path)?;
    let mut h = Fnv::new();
    h.write(path.to_string_lossy().as_bytes());
    h.write(&len.to_le_bytes());
    h.write(&mtime.to_le_bytes());
    h.write(&fps.to_le_bytes());
    h.write(&height.to_le_bytes());
    Ok(format!("v{:016x}", h.finish()))
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
