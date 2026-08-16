//! The preview's audio sink and the transport clock.
//!
//! Audio is the master clock: the output callback advances the playhead and video follows it.
//! Doing it the other way round is what makes editors drift and click. The callback never
//! allocates, never blocks on I/O, and holds a lock only long enough to clone an `Arc`.
//!
//! **No mixing happens here.** The callback asks `mix::AudioPlan` for a block of project samples
//! and its only remaining job is to get them to the device — which is the same relationship
//! `render.rs` has with the window. If a device will not run at 48 kHz the block is stepped
//! through on the way out, and that step is the one thing the export does not do, because a file
//! is always written at the project rate.

use crate::mix::{AudioLayer, AudioPlan};
use crate::project::SAMPLE_RATE;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use memmap2::Mmap;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::Arc;

pub struct AudioEngine {
    plan: Arc<Mutex<Arc<AudioPlan>>>,
    /// Playhead in project samples (48 kHz), written by the audio thread.
    position: Arc<AtomicI64>,
    playing: Arc<AtomicBool>,
    /// Peak level of the last block, as a fixed-point 0..10000, for the meters.
    peak_l: Arc<AtomicU32>,
    peak_r: Arc<AtomicU32>,
    loop_end: Arc<AtomicI64>,
    _stream: Option<cpal::Stream>,
    pub device_name: String,
    pub sample_rate: u32,
    pub error: Option<String>,
}

impl AudioEngine {
    pub fn new() -> Self {
        let plan: Arc<Mutex<Arc<AudioPlan>>> = Arc::new(Mutex::new(Arc::new(AudioPlan::default())));
        let position = Arc::new(AtomicI64::new(0));
        let playing = Arc::new(AtomicBool::new(false));
        let peak_l = Arc::new(AtomicU32::new(0));
        let peak_r = Arc::new(AtomicU32::new(0));
        let loop_end = Arc::new(AtomicI64::new(i64::MAX));

        let mut me = Self {
            plan: plan.clone(),
            position: position.clone(),
            playing: playing.clone(),
            peak_l: peak_l.clone(),
            peak_r: peak_r.clone(),
            loop_end: loop_end.clone(),
            _stream: None,
            device_name: "none".into(),
            sample_rate: SAMPLE_RATE,
            error: None,
        };

        match build_stream(plan, position, playing, peak_l, peak_r, loop_end) {
            Ok((stream, name, rate)) => {
                me.device_name = name;
                me.sample_rate = rate;
                me._stream = Some(stream);
            }
            Err(e) => {
                // No audio device is not fatal; the editor still works silently.
                me.error = Some(e);
            }
        }
        me
    }

    pub fn set_plan(&self, plan: AudioPlan) {
        *self.plan.lock() = Arc::new(plan);
    }
    pub fn position_samples(&self) -> i64 {
        self.position.load(Ordering::Relaxed)
    }
    pub fn set_position_samples(&self, s: i64) {
        self.position.store(s.max(0), Ordering::Relaxed);
    }
    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }
    pub fn set_playing(&self, p: bool) {
        self.playing.store(p, Ordering::Relaxed);
    }
    pub fn set_stop_at(&self, s: i64) {
        self.loop_end.store(s, Ordering::Relaxed);
    }
    /// Peak levels of the last mixed block, 0..1 per channel.
    pub fn peaks(&self) -> (f32, f32) {
        (
            self.peak_l.load(Ordering::Relaxed) as f32 / 10_000.0,
            self.peak_r.load(Ordering::Relaxed) as f32 / 10_000.0,
        )
    }
}

fn build_stream(
    plan: Arc<Mutex<Arc<AudioPlan>>>,
    position: Arc<AtomicI64>,
    playing: Arc<AtomicBool>,
    peak_l: Arc<AtomicU32>,
    peak_r: Arc<AtomicU32>,
    loop_end: Arc<AtomicI64>,
) -> Result<(cpal::Stream, String, u32), String> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| "no audio output device".to_string())?;
    let name = device.name().unwrap_or_else(|_| "output".into());

    let supported = device
        .supported_output_configs()
        .map_err(|e| e.to_string())?
        .collect::<Vec<_>>();

    // Prefer an exact 48 kHz stereo f32 config so no resampling is needed at all.
    let mut chosen: Option<cpal::SupportedStreamConfig> = None;
    for c in &supported {
        if c.sample_format() == cpal::SampleFormat::F32
            && c.channels() == 2
            && c.min_sample_rate().0 <= SAMPLE_RATE
            && c.max_sample_rate().0 >= SAMPLE_RATE
        {
            chosen = Some(c.clone().with_sample_rate(cpal::SampleRate(SAMPLE_RATE)));
            break;
        }
    }
    let config = match chosen {
        Some(c) => c,
        None => device.default_output_config().map_err(|e| e.to_string())?,
    };

    let sample_rate = config.sample_rate().0;
    let channels = config.channels() as usize;
    // Ratio of project samples to output samples, for the rare device that will not do 48 kHz.
    let step = SAMPLE_RATE as f64 / sample_rate as f64;
    let fmt = config.sample_format();
    let stream_config: cpal::StreamConfig = config.into();

    let mut active: Vec<AudioLayer> = Vec::with_capacity(16);
    // Mixed project samples, before they are stepped out to the device rate.
    let mut block: Vec<f32> = Vec::with_capacity(8192);

    let err_fn = |e| eprintln!("audio stream error: {e}");

    macro_rules! make {
        ($t:ty, $conv:expr) => {{
            device.build_output_stream(
                &stream_config,
                move |out: &mut [$t], _: &cpal::OutputCallbackInfo| {
                    let frames = out.len() / channels;
                    let is_playing = playing.load(Ordering::Relaxed);
                    if !is_playing {
                        for s in out.iter_mut() {
                            *s = $conv(0.0);
                        }
                        peak_l.store(0, Ordering::Relaxed);
                        peak_r.store(0, Ordering::Relaxed);
                        return;
                    }

                    let plan = plan.lock().clone();
                    let start = position.load(Ordering::Relaxed);
                    let need = (frames as f64 * step).ceil() as usize + 2;
                    if block.len() < need * 2 {
                        // Only ever grows, and only when the device changes its block size, so
                        // steady-state playback still allocates nothing.
                        block.resize(need * 2, 0.0);
                    }
                    plan.mix_into(start, &mut block[..need * 2], &mut active);

                    let mut pk_l = 0f32;
                    let mut pk_r = 0f32;
                    for f in 0..frames {
                        let i = ((f as f64 * step) as usize).min(need - 1) * 2;
                        let (l, r) = (block[i], block[i + 1]);
                        pk_l = pk_l.max(l.abs());
                        pk_r = pk_r.max(r.abs());
                        let base = f * channels;
                        for ch in 0..channels {
                            let v = if ch == 0 { l } else if ch == 1 { r } else { (l + r) * 0.5 };
                            out[base + ch] = $conv(v);
                        }
                    }

                    let advanced = start + (frames as f64 * step).round() as i64;
                    let stop = loop_end.load(Ordering::Relaxed);
                    if advanced >= stop {
                        position.store(stop, Ordering::Relaxed);
                        playing.store(false, Ordering::Relaxed);
                    } else {
                        position.store(advanced, Ordering::Relaxed);
                    }
                    peak_l.store((pk_l * 10_000.0) as u32, Ordering::Relaxed);
                    peak_r.store((pk_r * 10_000.0) as u32, Ordering::Relaxed);
                },
                err_fn,
                None,
            )
        }};
    }

    let stream = match fmt {
        cpal::SampleFormat::F32 => make!(f32, |v: f32| v),
        cpal::SampleFormat::I16 => make!(i16, |v: f32| (v * 32767.0) as i16),
        cpal::SampleFormat::U16 => make!(u16, |v: f32| ((v * 32767.0) as i32 + 32768) as u16),
        other => return Err(format!("unsupported audio sample format {other:?}")),
    }
    .map_err(|e| e.to_string())?;

    stream.play().map_err(|e| e.to_string())?;
    Ok((stream, name, sample_rate))
}

pub fn open_pcm(path: &std::path::Path) -> Option<Arc<Mmap>> {
    let f = std::fs::File::open(path).ok()?;
    let m = unsafe { Mmap::map(&f).ok()? };
    Some(Arc::new(m))
}

/// Loads the precomputed min/max envelope written at import time.
pub fn load_peaks(path: &std::path::Path) -> Option<Arc<Vec<(i16, i16)>>> {
    let bytes = std::fs::read(path).ok()?;
    let mut v = Vec::with_capacity(bytes.len() / 4);
    for c in bytes.chunks_exact(4) {
        v.push((
            i16::from_le_bytes([c[0], c[1]]),
            i16::from_le_bytes([c[2], c[3]]),
        ));
    }
    Some(Arc::new(v))
}
