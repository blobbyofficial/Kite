//! Decoded-frame cache with background prefetch.
//!
//! Every displayed frame is looked up here first. A hit costs a hash lookup and an `Arc` clone.
//! A miss costs one JPEG decode of a proxy-sized image — a few milliseconds — and the prefetch
//! workers keep the region around the playhead warm so misses are rare during playback.

use crate::framestore::FrameStore;
use crate::project::MediaId;
use crate::proxy::{segment_of, ProxyBuilder, SegmentState, SEG_FRAMES};
use crossbeam_channel::{Sender, TrySendError};
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGBA8.
    pub rgba: Vec<u8>,
}

impl DecodedFrame {
    fn bytes(&self) -> usize {
        self.rgba.len()
    }
}

type Key = (MediaId, u32);

struct Lru {
    map: HashMap<Key, Arc<DecodedFrame>>,
    order: VecDeque<Key>,
    bytes: usize,
    cap: usize,
}

impl Lru {
    fn new(cap: usize) -> Self {
        Self { map: HashMap::new(), order: VecDeque::new(), bytes: 0, cap }
    }
    fn get(&mut self, k: &Key) -> Option<Arc<DecodedFrame>> {
        let v = self.map.get(k)?.clone();
        if let Some(p) = self.order.iter().position(|x| x == k) {
            self.order.remove(p);
        }
        self.order.push_back(*k);
        Some(v)
    }
    fn peek(&self, k: &Key) -> Option<Arc<DecodedFrame>> {
        self.map.get(k).cloned()
    }
    fn insert(&mut self, k: Key, v: Arc<DecodedFrame>) {
        if self.map.contains_key(&k) {
            return;
        }
        self.bytes += v.bytes();
        self.map.insert(k, v);
        self.order.push_back(k);
        while self.bytes > self.cap {
            match self.order.pop_front() {
                Some(old) => {
                    if let Some(v) = self.map.remove(&old) {
                        self.bytes -= v.bytes();
                    }
                }
                None => break,
            }
        }
    }
    fn drop_media(&mut self, id: MediaId) {
        self.order.retain(|(m, _)| *m != id);
        self.map.retain(|(m, _), v| {
            if *m == id {
                self.bytes -= v.bytes();
                false
            } else {
                true
            }
        });
    }
}

struct PrefetchReq {
    media: MediaId,
    from: u32,
    count: u32,
    generation: u64,
}

pub struct FrameCache {
    /// One open store per built span, not per media item.
    stores: Mutex<HashMap<(MediaId, i64), Arc<FrameStore>>>,
    /// The most recent frame we managed to show for each item, so the picture holds steady while
    /// a span is still being prepared instead of dropping to black.
    last_good: Mutex<HashMap<MediaId, u32>>,
    pub builder: Arc<ProxyBuilder>,
    lru: Arc<Mutex<Lru>>,
    tx: Sender<PrefetchReq>,
    /// Bumped whenever the playhead jumps, so stale prefetch work can be abandoned.
    generation: Arc<AtomicU64>,
    pub decoded_this_session: AtomicU64,
}

impl FrameCache {
    pub fn new(cache_bytes: usize, workers: usize, builder: Arc<ProxyBuilder>) -> Arc<Self> {
        let (tx, rx) = crossbeam_channel::bounded::<PrefetchReq>(64);
        let lru = Arc::new(Mutex::new(Lru::new(cache_bytes)));
        let generation = Arc::new(AtomicU64::new(0));

        let me = Arc::new(Self {
            stores: Mutex::new(HashMap::new()),
            last_good: Mutex::new(HashMap::new()),
            builder,
            lru: lru.clone(),
            tx,
            generation: generation.clone(),
            decoded_this_session: AtomicU64::new(0),
        });

        for _ in 0..workers.max(1) {
            let rx = rx.clone();
            let weak = Arc::downgrade(&me);
            std::thread::Builder::new()
                .name("kite-prefetch".into())
                .spawn(move || {
                    while let Ok(req) = rx.recv() {
                        let Some(cache) = weak.upgrade() else { return };
                        for i in 0..req.count {
                            // A newer request means the playhead moved; drop this work.
                            if cache.generation.load(Ordering::Relaxed) != req.generation {
                                break;
                            }
                            let f = req.from + i;
                            if cache.lru.lock().peek(&(req.media, f)).is_some() {
                                continue;
                            }
                            // Warm the span that is coming up as well as the frames themselves.
                            cache.builder.request(req.media, segment_of(f as i64), false);
                            let _ = cache.decode_into_cache(req.media, f);
                        }
                    }
                })
                .expect("spawn prefetch worker");
        }
        me
    }

    pub fn forget(&self, media: MediaId) {
        self.builder.forget(media);
        self.stores.lock().retain(|(m, _), _| *m != media);
        self.last_good.lock().remove(&media);
        self.lru.lock().drop_media(media);
    }

    /// Opens the span containing `frame`, if it has been built.
    fn store_for(&self, media: MediaId, frame: i64) -> Option<Arc<FrameStore>> {
        let seg = segment_of(frame);
        if let Some(s) = self.stores.lock().get(&(media, seg)) {
            return Some(s.clone());
        }
        let src = self.builder.source(media)?;
        let path = src.segment_path(seg);
        if !path.is_file() {
            return None;
        }
        let s = Arc::new(FrameStore::open(&path).ok()?);
        self.stores.lock().insert((media, seg), s.clone());
        Some(s)
    }

    /// Whether the span holding this frame is ready, being built, or not started.
    pub fn segment_state(&self, media: MediaId, frame: u32) -> SegmentState {
        self.builder.state(media, segment_of(frame as i64))
    }

    /// The last frame shown for this item, used to hold the picture while a span builds.
    pub fn last_good(&self, media: MediaId) -> Option<Arc<DecodedFrame>> {
        let f = *self.last_good.lock().get(&media)?;
        self.lru.lock().peek(&(media, f))
    }

    /// Cache-only lookup. Used when we would rather show a slightly stale frame than block.
    pub fn peek(&self, media: MediaId, frame: u32) -> Option<Arc<DecodedFrame>> {
        self.lru.lock().peek(&(media, frame))
    }

    /// Returns the frame, decoding it on the calling thread if it is not cached.
    pub fn get(&self, media: MediaId, frame: u32) -> Option<Arc<DecodedFrame>> {
        if let Some(f) = self.lru.lock().get(&(media, frame)) {
            self.last_good.lock().insert(media, frame);
            return Some(f);
        }
        self.decode_into_cache(media, frame)
    }

    /// The frame if it is available, otherwise the most recent one we did manage to show.
    pub fn get_or_last(&self, media: MediaId, frame: u32) -> Option<Arc<DecodedFrame>> {
        self.get(media, frame).or_else(|| self.last_good(media))
    }

    fn decode_into_cache(&self, media: MediaId, frame: u32) -> Option<Arc<DecodedFrame>> {
        let f = frame as i64;
        let store = match self.store_for(media, f) {
            Some(s) => s,
            None => {
                // Not built yet: ask for it and let the caller show the previous frame.
                self.builder.request(media, segment_of(f), true);
                return None;
            }
        };
        let local = (f % SEG_FRAMES) as usize;
        let jpeg = store.jpeg(local)?;
        let decoded = decode_jpeg_rgba(jpeg)?;
        let arc = Arc::new(decoded);
        self.lru.lock().insert((media, frame), arc.clone());
        self.last_good.lock().insert(media, frame);
        self.decoded_this_session.fetch_add(1, Ordering::Relaxed);
        Some(arc)
    }

    /// Asks the workers to warm `count` frames starting at `from`. Never blocks; if the queue is
    /// full we simply skip, because the playhead will ask again next frame anyway.
    pub fn prefetch(&self, media: MediaId, from: u32, count: u32) {
        let generation = self.generation.load(Ordering::Relaxed);
        let req = PrefetchReq { media, from, count, generation };
        match self.tx.try_send(req) {
            Ok(()) | Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    /// Call when the playhead jumps so in-flight prefetch work is abandoned.
    pub fn invalidate_prefetch(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    pub fn stats(&self) -> (usize, usize) {
        let l = self.lru.lock();
        (l.map.len(), l.bytes)
    }
}

pub fn decode_jpeg_rgba(data: &[u8]) -> Option<DecodedFrame> {
    let opts = DecoderOptions::default()
        .jpeg_set_out_colorspace(ColorSpace::RGBA)
        .set_use_unsafe(true);
    let mut dec = JpegDecoder::new_with_options(ZCursor::new(data), opts);
    dec.decode_headers().ok()?;
    let info = dec.info()?;
    let size = dec.output_buffer_size()?;
    let mut buf = vec![0u8; size];
    dec.decode_into(&mut buf).ok()?;
    Some(DecodedFrame {
        width: info.width as u32,
        height: info.height as u32,
        rgba: buf,
    })
}
