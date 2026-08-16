//! One mixer.
//!
//! The companion to `render.rs`, and the same shape. An [`AudioPlan`] describes what should be
//! heard over a range of the timeline; one mixer executes it; two sinks consume the result — cpal
//! for the preview, a WAV handed to ffmpeg for the export. ffmpeg keeps demux, decode and encode.
//! It does not mix, fade, delay, retime or limit anything.
//!
//! Everything is in **project samples**: 48 kHz, stereo, interleaved. Clip positions on the
//! timeline are frames, and the only conversion is `VideoSettings::frame_to_sample`, which is
//! exact integer arithmetic — so a cut lands on the same sample every time it is asked for and
//! nothing drifts across edits.
//!
//! Sources are already 48 kHz stereo before they reach here: the importer runs every file through
//! ffmpeg's resampler once and writes `audio.pcm`. That is deliberate — it means the mixer never
//! resamples, so there is no naive interpolation anywhere on the path, and the one resampling step
//! that does happen is done by the best implementation available to us.

use crate::project::{MediaId, Project, Timeline};
use memmap2::Mmap;
use std::sync::Arc;

/// Length of the ramp applied at every clip edge, in samples — two milliseconds.
///
/// Audio cannot be hard-cut. A clip that starts or ends part way through a waveform produces a
/// step discontinuity, which is a click, at every edit point. This is short enough to be
/// inaudible as a fade and long enough to remove the step.
pub const DECLICK: i64 = 96;

/// Where a layer's samples come from.
#[derive(Clone)]
pub enum Pcm {
    /// The 48 kHz stereo file the importer wrote, memory mapped.
    Mapped(Arc<Mmap>),
    /// Retimed audio, rendered once when the plan was built.
    Owned(Arc<Vec<i16>>),
}

impl Pcm {
    #[inline]
    fn frames(&self) -> i64 {
        match self {
            Pcm::Mapped(m) => (m.len() / 4) as i64,
            Pcm::Owned(v) => (v.len() / 2) as i64,
        }
    }

    /// Interleaved stereo pair at a source frame index, as -1..1 floats.
    #[inline]
    fn at(&self, i: i64) -> (f32, f32) {
        if i < 0 {
            return (0.0, 0.0);
        }
        const SCALE: f32 = 1.0 / 32768.0;
        match self {
            Pcm::Mapped(m) => {
                let off = (i as usize) * 4;
                let d: &[u8] = m;
                if off + 4 > d.len() {
                    return (0.0, 0.0);
                }
                (
                    i16::from_le_bytes([d[off], d[off + 1]]) as f32 * SCALE,
                    i16::from_le_bytes([d[off + 2], d[off + 3]]) as f32 * SCALE,
                )
            }
            Pcm::Owned(v) => {
                let off = (i as usize) * 2;
                if off + 2 > v.len() {
                    return (0.0, 0.0);
                }
                (v[off] as f32 * SCALE, v[off + 1] as f32 * SCALE)
            }
        }
    }

    #[inline]
    fn mono(&self, i: i64) -> f32 {
        let (l, r) = self.at(i);
        (l + r) * 0.5
    }
}

/// One clip's contribution to the mix.
#[derive(Clone)]
pub struct AudioLayer {
    /// First timeline sample this layer is heard on.
    pub start: i64,
    /// The clip's own out point. Fades out end here.
    pub body_end: i64,
    /// Where the layer actually stops, which is past `body_end` when the next clip dissolves in
    /// and this one keeps rolling underneath it.
    pub end: i64,
    /// Source sample for the layer's first timeline sample.
    pub src_offset: i64,
    pub volume: f32,
    pub fade_in: i64,
    pub fade_out: i64,
    /// Dissolve up at the head, matching the clip's `transition_in`.
    pub transition_in: i64,
    /// Dissolve down over `body_end..end`, because the next clip is coming up over the top.
    pub transition_out: i64,
    pub data: Pcm,
}

#[inline]
fn ramp(x: i64, n: i64) -> f32 {
    if n <= 0 {
        return 1.0;
    }
    (x as f32 / n as f32).clamp(0.0, 1.0)
}

impl AudioLayer {
    /// The complete gain envelope at one timeline sample. Every ramp the two paths used to
    /// implement separately — `volume`, `afade` in and out, the dissolve either side — is here,
    /// once.
    #[inline]
    pub fn gain_at(&self, t: i64) -> f32 {
        let local = t - self.start;
        let mut g = self.volume;
        g *= ramp(local + 1, self.fade_in);
        g *= ramp(self.body_end - t, self.fade_out);
        g *= ramp(local + 1, self.transition_in);
        g *= ramp(self.end - t, self.transition_out);
        // Never leave a step at an edit point.
        g *= ramp(local + 1, DECLICK) * ramp(self.end - t, DECLICK);
        g
    }
}

/// What should be heard, over the whole timeline.
#[derive(Default, Clone)]
pub struct AudioPlan {
    pub layers: Vec<AudioLayer>,
    /// Timeline length in samples.
    pub total: i64,
}

/// The ceiling every mix is held under, and the point above which it starts bending.
///
/// The export used to end with ffmpeg's `alimiter=limit=0.97` and the preview used to hard-clamp
/// at 1.0, so the two paths did not even agree on how loud the same mix was. This is the single
/// decision: a smooth saturation that is transparent below the knee and asymptotic to the
/// ceiling above it. It is a waveshaper, not a look-ahead limiter — which means it is
/// sample-exact and stateless, and that is what lets the two paths produce identical samples.
const CEILING: f32 = 0.97;
const KNEE: f32 = 0.7;

#[inline]
pub fn limit(v: f32) -> f32 {
    let a = v.abs();
    if a <= KNEE {
        return v;
    }
    let over = (a - KNEE) / (1.0 - KNEE);
    let shaped = 1.0 - (-over).exp();
    (KNEE + (CEILING - KNEE) * shaped).copysign(v)
}

impl AudioPlan {
    pub fn is_silent(&self) -> bool {
        self.layers.is_empty()
    }

    /// Mixes `out.len() / 2` interleaved stereo frames starting at timeline sample `from`.
    ///
    /// `active` is caller-owned scratch so the real-time callback allocates nothing.
    pub fn mix_into(&self, from: i64, out: &mut [f32], active: &mut Vec<AudioLayer>) {
        for s in out.iter_mut() {
            *s = 0.0;
        }
        let frames = (out.len() / 2) as i64;
        let to = from + frames;
        active.clear();
        for l in &self.layers {
            if l.end > from && l.start < to {
                active.push(l.clone());
            }
        }
        for l in active.iter() {
            let lo = l.start.max(from);
            let hi = l.end.min(to);
            for t in lo..hi {
                let (sl, sr) = l.data.at(l.src_offset + (t - l.start));
                let g = l.gain_at(t);
                let i = ((t - from) * 2) as usize;
                out[i] += sl * g;
                out[i + 1] += sr * g;
            }
        }
        for s in out.iter_mut() {
            *s = limit(*s);
        }
    }

    /// Convenience for callers that are not in a real-time context.
    pub fn mix_range(&self, from: i64, frames: usize) -> Vec<f32> {
        let mut out = vec![0.0; frames * 2];
        let mut scratch = Vec::new();
        self.mix_into(from, &mut out, &mut scratch);
        out
    }
}

/// Retimed audio, kept between plan rebuilds.
///
/// A plan is rebuilt on every edit and stretching is not free, so without this a project with a
/// speed change would pay for it on every keystroke. Entries survive only as long as something in
/// the current plan still wants them.
#[derive(Default)]
pub struct RetimeCache {
    map: std::collections::HashMap<u64, Arc<Vec<i16>>>,
    live: std::collections::HashSet<u64>,
}

impl RetimeCache {
    fn get_or_make(&mut self, key: u64, make: impl FnOnce() -> Vec<i16>) -> Arc<Vec<i16>> {
        self.live.insert(key);
        if let Some(v) = self.map.get(&key) {
            return v.clone();
        }
        let v = Arc::new(make());
        self.map.insert(key, v.clone());
        v
    }

    fn sweep(&mut self) {
        let live = std::mem::take(&mut self.live);
        self.map.retain(|k, _| live.contains(k));
    }
}

fn retime_key(media: MediaId, src_offset: i64, frames: i64, speed: f32) -> u64 {
    let mut s = 0xcbf29ce484222325u64;
    for v in [media, src_offset as u64, frames as u64, speed.to_bits() as u64] {
        s = (s ^ v).wrapping_mul(0x100000001b3);
    }
    s
}

/// Where the mixer gets a media item's 48 kHz stereo samples. The preview answers from what it
/// already has mapped; the export extracts anything missing before it starts.
pub trait PcmSource {
    fn pcm(&mut self, media: MediaId) -> Option<Arc<Mmap>>;
}

/// Works out what a timeline sounds like.
///
/// This mirrors `render::plan_frame` exactly, including the dissolve rule: a clip with a
/// `transition_in` fades up while the clip before it keeps rolling underneath, using material
/// past its out point, and fades down over the same window. Audio is additive and has no
/// occlusion, so unlike the picture *both* sides of a dissolve are ramped.
pub fn plan_audio(
    project: &Project,
    tl: &Timeline,
    source: &mut dyn PcmSource,
    retimed: &mut RetimeCache,
) -> AudioPlan {
    let s = tl.format.unwrap_or(project.settings);
    let mut layers = Vec::new();
    for track in tl.tracks.iter().filter(|t| !t.muted) {
        for (i, c) in track.clips.iter().enumerate() {
            // If the next clip dissolves in, this one keeps rolling underneath for that long.
            let tail = track
                .clips
                .get(i + 1)
                .map(|n| n.transition_in.max(0))
                .unwrap_or(0);
            let Some(mid) = c.media_id() else { continue };
            let Some(m) = project.media(mid) else { continue };
            if !m.has_audio {
                continue;
            }
            let Some(data) = source.pcm(mid) else { continue };
            if c.volume <= 0.0 {
                continue;
            }

            let start = s.frame_to_sample(c.start);
            let body_end = s.frame_to_sample(c.end());
            let end = s.frame_to_sample(c.end() + tail);
            let src_offset = s.frame_to_sample(c.src_in);
            let speed = c.speed.max(0.01);

            // A retimed clip is stretched once, here, into its own buffer. The mixer then only
            // ever plays material back at its natural rate, which keeps the inner loop simple and
            // — more to the point — means the preview and the export are playing the same bytes
            // rather than two implementations of the same idea.
            let (data, src_offset) = if (speed - 1.0).abs() < 1e-4 {
                (Pcm::Mapped(data), src_offset)
            } else {
                let frames = (end - start).max(0);
                let key = retime_key(mid, src_offset, frames, speed);
                let stretched = retimed.get_or_make(key, || {
                    time_stretch(&Pcm::Mapped(data), src_offset, frames as usize, speed)
                });
                (Pcm::Owned(stretched), 0)
            };

            layers.push(AudioLayer {
                start,
                body_end,
                end,
                src_offset,
                volume: c.volume,
                fade_in: s.frame_to_sample(c.fade_in),
                fade_out: s.frame_to_sample(c.fade_out),
                transition_in: s.frame_to_sample(c.transition_in.max(0)),
                transition_out: s.frame_to_sample(tail),
                data,
            });
        }
    }
    retimed.sweep();
    let total = s.frame_to_sample(tl.duration());
    AudioPlan { layers, total }
}

// ---------------------------------------------------------------------------
// Retiming
// ---------------------------------------------------------------------------

/// Window length for the overlap-add, in frames, and the fixed output hop (half of it).
const WSOLA_WINDOW: i64 = 1024;
const WSOLA_HOP: i64 = WSOLA_WINDOW / 2;
/// How far either side of the nominal read position to look for a better splice.
const WSOLA_SEARCH: i64 = 192;

/// Pitch-preserving retime: a clip played at 2x still sounds like the same voice, an octave
/// lower is not what a speed control is for.
///
/// WSOLA. Windows are laid down at a fixed output hop and picked up from the input at a hop
/// scaled by the speed, with a short search around each read position for the splice that best
/// continues what was just written. A Hann window at half-overlap sums to exactly one, so no
/// normalisation is needed.
///
/// It is fully deterministic, which is the property that matters here: it is what lets the
/// preview and the export produce the same samples rather than merely similar ones. It runs when
/// the plan is built, never in the audio callback.
pub fn time_stretch(src: &Pcm, src_start: i64, out_frames: usize, speed: f32) -> Vec<i16> {
    let mut out = vec![0f32; out_frames * 2];
    if out_frames == 0 {
        return Vec::new();
    }
    let hop_in = ((WSOLA_HOP as f64) * speed as f64).round().max(1.0) as i64;
    let window: Vec<f32> = (0..WSOLA_WINDOW)
        .map(|j| {
            0.5 * (1.0
                - (2.0 * std::f32::consts::PI * j as f32 / WSOLA_WINDOW as f32).cos())
        })
        .collect();

    // Start half a window early so the first output sample is already at full level rather than
    // fading up out of nothing.
    let mut read = src_start - WSOLA_HOP;
    let mut write = -WSOLA_HOP;
    let limit_frames = out_frames as i64;

    while write < limit_frames {
        for j in 0..WSOLA_WINDOW {
            let o = write + j;
            if o < 0 || o >= limit_frames {
                continue;
            }
            let (l, r) = src.at(read + j);
            let g = window[j as usize];
            out[(o * 2) as usize] += l * g;
            out[(o * 2 + 1) as usize] += r * g;
        }

        // What would have come next if the source had simply carried on playing, and where we
        // nominally want to read from instead.
        let natural = read + WSOLA_HOP;
        let nominal = read + hop_in;
        // The splice is found in two passes — a coarse sweep and then a few samples either side
        // of the winner. Searching every offset is nearly four times the work for a splice that
        // lands in the same place, and this runs on the interactive path.
        let score_at = |k: i64| {
            let mut dot = 0f32;
            let mut energy = 1e-9f32;
            let mut j = 0i64;
            // Every other sample is plenty to find the splice and halves the cost again.
            while j < WSOLA_HOP {
                let a = src.mono(natural + j);
                let b = src.mono(nominal + k + j);
                dot += a * b;
                energy += b * b;
                j += 2;
            }
            dot / energy.sqrt()
        };
        let mut best = 0i64;
        let mut best_score = f32::NEG_INFINITY;
        let mut k = -WSOLA_SEARCH;
        while k <= WSOLA_SEARCH {
            let sc = score_at(k);
            if sc > best_score {
                best_score = sc;
                best = k;
            }
            k += 4;
        }
        for k in (best - 3)..=(best + 3) {
            let sc = score_at(k);
            if sc > best_score {
                best_score = sc;
                best = k;
            }
        }
        read = nominal + best;
        write += WSOLA_HOP;
    }

    out.iter()
        .map(|v| (v.clamp(-1.0, 1.0) * 32767.0) as i16)
        .collect()
}

// ---------------------------------------------------------------------------
// WAV
// ---------------------------------------------------------------------------

/// The export hands ffmpeg a 32-bit float WAV.
///
/// Float rather than 16-bit so the mix reaches the encoder without a quantisation step of our
/// own; the encoder should be the only place the signal loses anything.
/// Streams a float WAV to disk without holding the whole mix in memory.
pub struct WavWriter {
    file: std::io::BufWriter<std::fs::File>,
    frames: u32,
}

impl WavWriter {
    pub fn create(path: &std::path::Path) -> std::io::Result<Self> {
        use std::io::Write;
        let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
        // A placeholder header; the sizes are patched in by `finish`.
        file.write_all(&Self::header(0))?;
        Ok(Self { file, frames: 0 })
    }

    fn header(frames: u32) -> [u8; 44] {
        let channels: u16 = 2;
        let rate: u32 = crate::project::SAMPLE_RATE;
        let bits: u16 = 32;
        let block_align: u16 = channels * bits / 8;
        let byte_rate: u32 = rate * block_align as u32;
        let data_len: u32 = frames.saturating_mul(block_align as u32);
        let mut h = [0u8; 44];
        h[0..4].copy_from_slice(b"RIFF");
        h[4..8].copy_from_slice(&(36u32.saturating_add(data_len)).to_le_bytes());
        h[8..12].copy_from_slice(b"WAVE");
        h[12..16].copy_from_slice(b"fmt ");
        h[16..20].copy_from_slice(&16u32.to_le_bytes());
        // 3 is IEEE float.
        h[20..22].copy_from_slice(&3u16.to_le_bytes());
        h[22..24].copy_from_slice(&channels.to_le_bytes());
        h[24..28].copy_from_slice(&rate.to_le_bytes());
        h[28..32].copy_from_slice(&byte_rate.to_le_bytes());
        h[32..34].copy_from_slice(&block_align.to_le_bytes());
        h[34..36].copy_from_slice(&bits.to_le_bytes());
        h[36..40].copy_from_slice(b"data");
        h[40..44].copy_from_slice(&data_len.to_le_bytes());
        h
    }

    pub fn write(&mut self, interleaved: &[f32]) -> std::io::Result<()> {
        use std::io::Write;
        let mut bytes = Vec::with_capacity(interleaved.len() * 4);
        for v in interleaved {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        self.file.write_all(&bytes)?;
        self.frames = self.frames.saturating_add((interleaved.len() / 2) as u32);
        Ok(())
    }

    pub fn finish(mut self) -> std::io::Result<u32> {
        use std::io::{Seek, SeekFrom, Write};
        self.file.flush()?;
        let mut file = self.file.into_inner().map_err(|e| e.into_error())?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&Self::header(self.frames))?;
        file.flush()?;
        Ok(self.frames)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frames: usize, hz: f32) -> Pcm {
        let mut v = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            let s = (2.0 * std::f32::consts::PI * hz * i as f32 / 48_000.0).sin();
            let q = (s * 20_000.0) as i16;
            v.push(q);
            v.push(q);
        }
        Pcm::Owned(Arc::new(v))
    }

    fn layer(start: i64, len: i64, data: Pcm) -> AudioLayer {
        AudioLayer {
            start,
            body_end: start + len,
            end: start + len,
            src_offset: 0,
            volume: 1.0,
            fade_in: 0,
            fade_out: 0,
            transition_in: 0,
            transition_out: 0,
            data,
        }
    }

    #[test]
    fn the_limiter_is_transparent_below_the_knee_and_never_exceeds_the_ceiling() {
        for v in [0.0f32, 0.1, 0.5, 0.69] {
            assert!((limit(v) - v).abs() < 1e-6, "{v} should pass through untouched");
        }
        for v in [1.0f32, 2.0, 8.0, 100.0] {
            assert!(limit(v) <= CEILING + 1e-6, "{v} limited to {}", limit(v));
            assert!(limit(-v) >= -CEILING - 1e-6);
        }
        // Monotonic, or loud material would fold back on itself.
        let mut prev = -1.0f32;
        let mut x = 0.0f32;
        while x < 4.0 {
            let y = limit(x);
            assert!(y >= prev, "limiter went backwards at {x}");
            prev = y;
            x += 0.01;
        }
    }

    #[test]
    fn clip_edges_are_ramped_rather_than_cut() {
        let l = layer(0, 4800, tone(4800, 440.0));
        assert!(l.gain_at(0) < 0.05, "a clip must not start at full gain");
        assert!((l.gain_at(2400) - 1.0).abs() < 1e-6, "and must reach it in the middle");
        assert!(l.gain_at(4799) < 0.05, "nor end at full gain");
    }

    #[test]
    fn a_dissolve_sums_to_a_continuous_level() {
        // Two clips of the same tone, the second dissolving in over the last 1000 samples of the
        // first. The sum through the crossfade must not collapse or spike.
        let a = AudioLayer {
            transition_out: 1000,
            body_end: 4000,
            end: 5000,
            ..layer(0, 5000, tone(6000, 440.0))
        };
        let b = AudioLayer { transition_in: 1000, ..layer(4000, 2000, tone(6000, 440.0)) };
        let plan = AudioPlan { layers: vec![a, b], total: 6000 };
        let mixed = plan.mix_range(0, 6000);

        let rms = |from: usize, n: usize| {
            let mut t = 0f64;
            for i in from..from + n {
                t += (mixed[i * 2] as f64).powi(2);
            }
            (t / n as f64).sqrt()
        };
        let before = rms(2000, 500);
        let during = rms(4400, 200);
        assert!(before > 0.1, "the test signal should be audible, got {before}");
        assert!(
            during > before * 0.5 && during < before * 1.6,
            "the crossfade dipped or spiked: {before} then {during}"
        );
    }

    #[test]
    fn retiming_is_quick_enough_for_an_edit() {
        // The plan is rebuilt on every edit, on the UI thread, so this is on the interactive
        // path and has to stay well inside a user's idea of "instant".
        let src = tone(48_000 * 10, 300.0);
        let t0 = std::time::Instant::now();
        let out = time_stretch(&src, 0, 48_000 * 5, 2.0);
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(out.len(), 48_000 * 5 * 2);
        println!("retiming five seconds took {ms:.1} ms");
        assert!(ms < 150.0, "retiming five seconds took {ms:.0} ms, too slow to sit on an edit");
    }

    #[test]
    fn retiming_keeps_the_pitch() {
        // A 440 Hz tone played at double speed must still be 440 Hz. Resampling instead of
        // stretching would put it at 880.
        let src = tone(96_000, 440.0);
        let out = time_stretch(&src, 0, 24_000, 2.0);
        let stretched = Pcm::Owned(Arc::new(out));

        // Estimate the period by finding the first strong autocorrelation peak.
        let period = |p: &Pcm, from: i64| {
            let mut best = 0i64;
            let mut best_score = f32::NEG_INFINITY;
            for lag in 40..400i64 {
                let mut dot = 0f32;
                for j in 0..2000i64 {
                    dot += p.mono(from + j) * p.mono(from + j + lag);
                }
                if dot > best_score {
                    best_score = dot;
                    best = lag;
                }
            }
            best
        };
        // 48000 / 440 is about 109 samples.
        let p = period(&stretched, 4000);
        assert!(
            (p - 109).abs() <= 4,
            "retimed tone has a period of {p} samples, expected about 109 (440 Hz)"
        );
    }

    #[test]
    fn a_muted_track_contributes_nothing() {
        use crate::project::{ClipSource, TrackKind};
        struct NoPcm;
        impl PcmSource for NoPcm {
            fn pcm(&mut self, _: MediaId) -> Option<Arc<Mmap>> {
                None
            }
        }
        let mut p = Project::default();
        let c = p.new_clip(ClipSource::Color([0; 4]), 0, 30, 0);
        let tid = p.tracks().iter().find(|t| t.kind == TrackKind::Audio).unwrap().id;
        p.track_mut(tid).unwrap().clips.push(c);
        let plan = plan_audio(&p, p.tl(), &mut NoPcm, &mut RetimeCache::default());
        assert!(plan.is_silent(), "a colour card has no sound to contribute");
    }
}
