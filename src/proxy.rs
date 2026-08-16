//! On-demand, segmented playback files.
//!
//! The first version transcoded a whole file the moment you imported it, which is the wrong shape:
//! importing a two hour recording to use thirty seconds of it meant waiting for two hours of
//! transcoding. Instead the source is divided into fixed spans of frames, and a span is built only
//! when the timeline actually asks for a frame inside it. Editing starts the moment the file is
//! probed.

use crate::ffmpeg::{self, Tools};
use crate::framestore::{scan_frames, FrameStore, FrameStoreWriter};
use crate::project::MediaId;
use anyhow::{bail, Context, Result};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Frames per span. Five seconds at 30fps: long enough that ffmpeg's startup cost is amortised,
/// short enough that a span appears quickly after you land on it.
pub const SEG_FRAMES: i64 = 150;

pub fn segment_of(frame: i64) -> i64 {
    if frame < 0 {
        0
    } else {
        frame / SEG_FRAMES
    }
}

/// Everything needed to build any span of one media item on demand.
#[derive(Clone)]
pub struct ProxySource {
    pub media: MediaId,
    pub path: PathBuf,
    pub dir: PathBuf,
    pub fps: u32,
    pub height: u32,
    pub total_frames: i64,
}

impl ProxySource {
    pub fn segment_path(&self, seg: i64) -> PathBuf {
        self.dir.join(format!("seg{seg:05}.kfs"))
    }
    pub fn segment_count(&self) -> i64 {
        (self.total_frames + SEG_FRAMES - 1) / SEG_FRAMES
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SegmentState {
    Missing,
    Building,
    Ready,
    Failed,
}

struct Queue {
    /// Nearest-to-the-playhead first; the UI re-requests every frame so ordering stays fresh.
    pending: VecDeque<(MediaId, i64)>,
    inflight: HashSet<(MediaId, i64)>,
    failed: HashSet<(MediaId, i64)>,
}

pub struct ProxyBuilder {
    sources: Mutex<HashMap<MediaId, ProxySource>>,
    queue: Mutex<Queue>,
    signal: (crossbeam_channel::Sender<()>, crossbeam_channel::Receiver<()>),
    pub built: AtomicU64,
}

impl ProxyBuilder {
    pub fn new(tools: Arc<Tools>, workers: usize) -> Arc<Self> {
        let me = Arc::new(Self {
            sources: Mutex::new(HashMap::new()),
            queue: Mutex::new(Queue {
                pending: VecDeque::new(),
                inflight: HashSet::new(),
                failed: HashSet::new(),
            }),
            signal: crossbeam_channel::bounded(256),
            built: AtomicU64::new(0),
        });

        for _ in 0..workers.max(1) {
            let weak = Arc::downgrade(&me);
            let tools = tools.clone();
            std::thread::Builder::new()
                .name("kite-proxy".into())
                .spawn(move || loop {
                    let Some(b) = weak.upgrade() else { return };
                    let rx = b.signal.1.clone();
                    let job = b.take_job();
                    drop(b);
                    match job {
                        Some((source, seg)) => {
                            let Some(b) = weak.upgrade() else { return };
                            let r = build_segment(&tools, &source, seg);
                            let mut q = b.queue.lock();
                            q.inflight.remove(&(source.media, seg));
                            if r.is_err() {
                                q.failed.insert((source.media, seg));
                            } else {
                                b.built.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        None => {
                            // Nothing to do; park until something is requested.
                            let _ = rx.recv_timeout(std::time::Duration::from_millis(250));
                        }
                    }
                })
                .expect("spawn proxy worker");
        }
        me
    }

    pub fn register(&self, source: ProxySource) {
        self.sources.lock().insert(source.media, source);
    }
    pub fn forget(&self, media: MediaId) {
        self.sources.lock().remove(&media);
        let mut q = self.queue.lock();
        q.pending.retain(|(m, _)| *m != media);
        q.failed.retain(|(m, _)| *m != media);
    }
    pub fn source(&self, media: MediaId) -> Option<ProxySource> {
        self.sources.lock().get(&media).cloned()
    }

    pub fn state(&self, media: MediaId, seg: i64) -> SegmentState {
        let Some(src) = self.source(media) else { return SegmentState::Missing };
        if src.segment_path(seg).is_file() {
            return SegmentState::Ready;
        }
        let q = self.queue.lock();
        if q.inflight.contains(&(media, seg)) {
            SegmentState::Building
        } else if q.failed.contains(&(media, seg)) {
            SegmentState::Failed
        } else {
            SegmentState::Missing
        }
    }

    /// Asks for a span. Cheap to call every frame: already-queued and already-built spans are
    /// dropped, and a fresh request jumps the queue so the playhead is always served first.
    pub fn request(&self, media: MediaId, seg: i64, urgent: bool) {
        let Some(src) = self.source(media) else { return };
        if seg < 0 || seg >= src.segment_count() || src.segment_path(seg).is_file() {
            return;
        }
        {
            let mut q = self.queue.lock();
            let key = (media, seg);
            if q.inflight.contains(&key) || q.failed.contains(&key) {
                return;
            }
            if let Some(pos) = q.pending.iter().position(|k| *k == key) {
                if urgent && pos > 0 {
                    q.pending.remove(pos);
                    q.pending.push_front(key);
                }
                return;
            }
            if urgent {
                q.pending.push_front(key);
            } else {
                q.pending.push_back(key);
            }
            // Do not let a long timeline queue unbounded work.
            while q.pending.len() > 64 {
                q.pending.pop_back();
            }
        }
        let _ = self.signal.0.try_send(());
    }

    fn take_job(&self) -> Option<(ProxySource, i64)> {
        let mut q = self.queue.lock();
        let (media, seg) = q.pending.pop_front()?;
        q.inflight.insert((media, seg));
        drop(q);
        self.source(media).map(|s| (s, seg))
    }

    /// How much of the spans a clip covers is ready, 0..1, for the timeline's progress hint.
    pub fn coverage(&self, media: MediaId, from: i64, to: i64) -> f32 {
        let Some(src) = self.source(media) else { return 0.0 };
        let first = segment_of(from.max(0));
        let last = segment_of((to - 1).max(0)).min(src.segment_count().saturating_sub(1));
        if last < first {
            return 1.0;
        }
        let mut ready = 0;
        let mut total = 0;
        for seg in first..=last {
            total += 1;
            if src.segment_path(seg).is_file() {
                ready += 1;
            }
        }
        if total == 0 {
            1.0
        } else {
            ready as f32 / total as f32
        }
    }

    pub fn pending_count(&self) -> usize {
        let q = self.queue.lock();
        q.pending.len() + q.inflight.len()
    }
}

/// Transcodes one span into an indexed all-intra store.
fn build_segment(tools: &Tools, src: &ProxySource, seg: i64) -> Result<()> {
    let out = src.segment_path(seg);
    if out.is_file() {
        return Ok(());
    }
    std::fs::create_dir_all(&src.dir).ok();

    let start = seg * SEG_FRAMES;
    let start_s = start as f64 / src.fps as f64;
    let span_s = SEG_FRAMES as f64 / src.fps as f64;
    let scale = format!(
        "scale=-2:{}:flags=fast_bilinear,fps={}",
        src.height, src.fps
    );

    // -ss before -i seeks on the container, which is what makes building one span cheap
    // regardless of how far into a long recording it sits.
    let mut child = ffmpeg::command(&tools.ffmpeg)
        .args(["-v", "error", "-nostdin", "-ss", &format!("{start_s:.6}")])
        .arg("-i")
        .arg(&src.path)
        .args(["-t", &format!("{span_s:.6}")])
        .args(["-an", "-sn", "-dn", "-vf", &scale])
        .args(["-c:v", "mjpeg", "-q:v", "6", "-pix_fmt", "yuvj420p"])
        .args(["-f", "image2pipe", "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting ffmpeg for a playback span")?;

    let mut stdout = child.stdout.take().expect("piped stdout");
    // Written to a temp name and renamed, so a half-built span is never mistaken for a ready one.
    let tmp = out.with_extension("part");
    let mut writer = FrameStoreWriter::create(&tmp, src.fps)?;
    let mut buf: Vec<u8> = Vec::with_capacity(1 << 20);
    let mut chunk = vec![0u8; 256 * 1024];
    let mut count = 0i64;

    loop {
        let n = stdout.read(&mut chunk).context("reading ffmpeg output")?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        let (ranges, consumed) = scan_frames(&buf);
        for r in ranges {
            if count >= SEG_FRAMES {
                break;
            }
            writer.push(&buf[r])?;
            count += 1;
        }
        buf.drain(..consumed);
    }

    let status = child.wait().context("waiting for ffmpeg")?;
    if !status.success() && count == 0 {
        let mut err = String::new();
        if let Some(mut e) = child.stderr.take() {
            e.read_to_string(&mut err).ok();
        }
        std::fs::remove_file(&tmp).ok();
        bail!(
            "could not build playback span {seg}: {}",
            err.lines().next().unwrap_or("unknown error")
        );
    }
    writer.finish()?;
    if count == 0 {
        std::fs::remove_file(&tmp).ok();
        bail!("playback span {seg} produced no frames");
    }
    std::fs::rename(&tmp, &out).context("publishing the playback span")?;
    Ok(())
}

/// Opens a built span for reading.
pub fn open_segment(path: &Path) -> Option<FrameStore> {
    FrameStore::open(path).ok()
}
