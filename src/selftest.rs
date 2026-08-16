//! End-to-end verification of the media pipeline, runnable from CI with `kite --selftest`.
//!
//! It synthesises a real clip with ffmpeg, imports it exactly as the app does, decodes frames out
//! of the proxy store, builds a small timeline and exports it — then checks the result. This
//! exercises every part of the pipeline that does not need a window.

use crate::decode::FrameCache;
use crate::export::{self, Encoder, ExportMsg, ExportSettings, Quality};
use crate::ffmpeg::{self, Tools};
use crate::import::{ImportMsg, Importer};
use crate::project::{ClipSource, ImportState, MediaItem, Project, TextProps, TrackKind};
use anyhow::{bail, Context, Result};

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

macro_rules! step {
    ($($a:tt)*) => { println!("  ..  {}", format!($($a)*)) };
}
macro_rules! pass {
    ($($a:tt)*) => { println!("  OK  {}", format!($($a)*)) };
}

pub fn run(tools: Arc<Tools>) -> Result<()> {
    println!("Kite self-test");
    println!("  ffmpeg  {}", tools.ffmpeg.display());
    println!("  ffprobe {}", tools.ffprobe.display());

    gpu_backend_check()?;

    let dir = std::env::temp_dir().join(format!("kite-selftest-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let src = dir.join("source.mp4");
    let out = dir.join("export.mp4");

    // ---- 1. make a source file -------------------------------------------
    step!("synthesising a 4 second 1280x720 test clip");
    let status = ffmpeg::command(&tools.ffmpeg)
        .args(["-v", "error", "-y"])
        .args(["-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=30"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"])
        .args(["-t", "4", "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p"])
        .args(["-c:a", "aac", "-shortest"])
        .arg(&src)
        .status()
        .context("running ffmpeg to build the test clip")?;
    if !status.success() {
        bail!("could not synthesise the test clip");
    }
    pass!("source clip {} bytes", std::fs::metadata(&src)?.len());

    // ---- 2. probe ---------------------------------------------------------
    let info = ffmpeg::probe(&tools, &src)?;
    if !info.has_video || !info.has_audio {
        bail!("probe did not report both streams: {info:?}");
    }
    if info.width != 1280 || info.height != 720 {
        bail!("probe reported {}x{}, expected 1280x720", info.width, info.height);
    }
    pass!("probe: {}x{} {:.2}s {:.2} fps", info.width, info.height, info.duration, info.fps);

    // ---- 3. import (proxy + audio + peaks) --------------------------------
    let mut project = Project::default();
    let media_id = project.alloc_id();
    let importer = Importer::new(tools.clone(), dir.join("cache"), 2);
    step!("probing and preparing");
    let t0 = Instant::now();
    importer.submit(media_id, src.clone(), project.seq().fps, 540);

    let mut video_dir = None;
    let mut audio_path = None;
    let mut peaks_path = None;
    let mut frames = 0i64;
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut probed_ms = 0u128;
    loop {
        if Instant::now() > deadline {
            bail!("import timed out");
        }
        match importer.rx.recv_timeout(Duration::from_secs(5)) {
            Ok(ImportMsg::Probed { info: i2, frames: f, video_dir: d, .. }) => {
                probed_ms = t0.elapsed().as_millis();
                frames = f;
                video_dir = Some(d);
                if !i2.has_video {
                    bail!("probe lost the video stream");
                }
            }
            Ok(ImportMsg::AudioReady { audio, peaks, .. }) => {
                audio_path = Some(audio);
                peaks_path = Some(peaks);
                break;
            }
            Ok(ImportMsg::Failed { error, .. }) => bail!("import failed: {error}"),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(e) => bail!("import channel closed: {e}"),
        }
    }
    if !(115..=125).contains(&frames) {
        bail!("expected about 120 frames at 30 fps, got {frames}");
    }
    // Editing must be possible almost immediately; only the sound waits.
    if probed_ms > 3000 {
        bail!("probing took {probed_ms} ms, which is too long to call the clip editable");
    }
    pass!("probed in {probed_ms} ms, {frames} frames, editable straight away");

    // ---- 4. spans are built on demand ------------------------------------
    let builder = crate::proxy::ProxyBuilder::new(tools.clone(), 2);
    builder.register(crate::proxy::ProxySource {
        media: media_id,
        path: src.clone(),
        dir: video_dir.clone().context("no span directory")?,
        fps: project.seq().fps,
        height: 540,
        total_frames: frames,
    });
    let cache = FrameCache::new(64 * 1024 * 1024, 2, builder.clone());

    // Nothing is built yet, so the first ask must not block — it should return nothing and
    // queue the work.
    if cache.get(media_id, 0).is_some() {
        bail!("a span was somehow ready before anything asked for it");
    }
    let t0 = Instant::now();
    let mut first = None;
    while Instant::now() < t0 + Duration::from_secs(60) {
        if let Some(f) = cache.get(media_id, 0) {
            first = Some(f);
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let first = first.context("the first span never became available")?;
    let span_ms = t0.elapsed().as_millis();
    if first.height != 540 {
        bail!("span decoded at height {}", first.height);
    }
    pass!("first span ready in {span_ms} ms, {}x{}", first.width, first.height);

    on_demand_check(&tools, &dir)?;

    let t0 = Instant::now();
    for f in 0..30u32 {
        cache.get(media_id, f).context("sequential decode failed")?;
    }
    let seq = t0.elapsed().as_secs_f64() * 1000.0 / 30.0;
    pass!("sequential decode {seq:.2} ms/frame");

    let audio_file = audio_path.context("no audio track was extracted")?;
    let audio_for_mix = audio_file.clone();
    let audio_bytes = std::fs::metadata(&audio_file)?.len();
    let expect = 4.0 * 48_000.0 * 4.0;
    if (audio_bytes as f64) < expect * 0.8 {
        bail!("audio is {audio_bytes} bytes, expected around {expect}");
    }
    let peaks = crate::audio::load_peaks(&peaks_path.context("no peaks file")?)
        .context("peaks file did not load")?;
    if peaks.len() < 100 || peaks.iter().all(|(lo, hi)| *lo == 0 && *hi == 0) {
        bail!("waveform peaks look empty ({} buckets)", peaks.len());
    }
    pass!("audio {audio_bytes} bytes, {} waveform buckets", peaks.len());

    // ---- 5. build a timeline ---------------------------------------------
    project.media.push(MediaItem {
        id: media_id,
        path: src.clone(),
        name: "source.mp4".into(),
        duration: info.duration,
        frames,
        src_width: info.width,
        src_height: info.height,
        src_fps: info.fps,
        has_video: true,
        has_audio: true,
        audio_path: Some(audio_file),
        peaks_path: None,
        state: ImportState::Ready,
        error: None,
        bin: 0,
    });

    let v_track = project
        .tracks()
        .iter()
        .filter(|t| t.kind == TrackKind::Video)
        .last()
        .map(|t| t.id)
        .context("no video track")?;
    let v_top = project
        .tracks()
        .iter()
        .find(|t| t.kind == TrackKind::Video)
        .map(|t| t.id)
        .context("no upper video track")?;

    // Two cuts from the same source, so trimming and source offsets are covered.
    let mut a = project.new_clip(ClipSource::Media(media_id), 0, 45, 0);
    a.fade_in = 5;
    let mut b = project.new_clip(ClipSource::Media(media_id), 45, 45, 70);
    b.volume = 0.5;
    b.fade_out = 6;
    // Dissolve into the second clip; clip a has plenty of material past its out point.
    b.transition_in = 10;
    let mut pip = project.new_clip(ClipSource::Media(media_id), 20, 30, 10);
    pip.scale = 0.35;
    pip.pos_x = 0.3;
    pip.pos_y = -0.3;
    pip.opacity = 0.9;
    pip.color = crate::project::ColorAdjust { brightness: 0.02, contrast: 1.15, saturation: 1.2 };
    let text = project.new_clip(
        ClipSource::Text(TextProps { text: "Kite self-test".into(), ..Default::default() }),
        10,
        40,
        0,
    );

    project.track_mut(v_track).unwrap().clips.push(a);
    project.track_mut(v_track).unwrap().clips.push(b);
    project.track_mut(v_top).unwrap().clips.push(pip);
    project.track_mut(v_top).unwrap().clips.push(text);
    project.normalize();

    if project.duration() != 90 {
        bail!("timeline duration is {} frames, expected 90", project.duration());
    }
    pass!("timeline: {} clips, {} frames", project.tracks().iter().map(|t| t.clips.len()).sum::<usize>(), project.duration());

    // Round-trip the document to make sure projects actually reload.
    let pfile = dir.join("test.kite");
    project.save(&pfile)?;
    let reloaded = Project::load(&pfile)?;
    if reloaded.duration() != project.duration() || reloaded.media.len() != project.media.len() {
        bail!("project did not survive a save/load round trip");
    }
    pass!("project save/load round trip");

    // ---- 6. export --------------------------------------------------------
    let settings = ExportSettings {
        path: out.clone(),
        encoder: Encoder::X264,
        quality: Quality::Balanced,
        width: project.seq().width,
        height: project.seq().height,
        fps: project.seq().fps,
        include_audio: true,
    };
    let t0 = Instant::now();
    let job = export::start(tools.clone(), project.clone(), project.tl().id, settings.clone());
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        if Instant::now() > deadline {
            bail!("export timed out");
        }
        match job.rx.recv_timeout(Duration::from_secs(10)) {
            Ok(ExportMsg::Done { .. }) => break,
            Ok(ExportMsg::Failed(e)) => bail!("export failed: {e}"),
            Ok(ExportMsg::Progress { .. }) => {}
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(e) => bail!("export channel closed: {e}"),
        }
    }
    let export_ms = t0.elapsed().as_millis();

    let result = ffmpeg::probe(&tools, &out)?;
    if !result.has_video || !result.has_audio {
        bail!("exported file is missing a stream: {result:?}");
    }
    if result.width != 1920 || result.height != 1080 {
        bail!("exported {}x{}, expected 1920x1080", result.width, result.height);
    }
    let expected_secs = 90.0 / 30.0;
    if (result.duration - expected_secs).abs() > 0.4 {
        bail!("exported duration {:.2}s, expected {expected_secs:.2}s", result.duration);
    }
    let size = std::fs::metadata(&out)?.len();
    pass!(
        "export {}x{} {:.2}s, {} KB, in {export_ms} ms",
        result.width,
        result.height,
        result.duration,
        size / 1024
    );

    // ---- 7. does a crossfade actually dissolve? ----------------------------
    dissolve_check(&tools, &dir, &src, info.duration)?;

    // ---- 7b. speed changes --------------------------------------------------
    speed_check(&tools, &dir, &src, info.duration)?;

    // ---- 7c. one renderer: the preview and the export must agree ----------
    parity_check(&tools, &dir, &src, info.duration, media_id, &cache)?;

    // ---- 7c2. one mixer: the preview and the export must agree ------------
    audio_parity_check(&tools, &dir, &src, info.duration, media_id, &audio_for_mix)?;

    // ---- 7d. the interactive path has to stay inside its budget -----------
    preview_budget_check(&cache, media_id, &src, info.duration)?;

    // ---- 8. a graph big enough to have overflowed a command line ----------
    big_graph_check(&tools, &dir, &src, info.duration)?;

    // ---- 9. does the timeline model scale? --------------------------------
    scale_check()?;

    std::fs::remove_dir_all(&dir).ok();
    println!("\nAll checks passed.");
    Ok(())
}

/// Renders a saved project from the command line, printing the filtergraph it built.
pub fn export_cli(tools: Arc<Tools>, project_path: &str, out: &str) -> Result<()> {
    let project = Project::load(std::path::Path::new(project_path))
        .with_context(|| format!("loading {project_path}"))?;
    let settings = ExportSettings {
        path: std::path::PathBuf::from(out),
        encoder: Encoder::X264,
        quality: Quality::High,
        width: project.seq().width,
        height: project.seq().height,
        fps: project.seq().fps,
        include_audio: true,
    };
    let (clips, silent, muted) = export::audio_summary(&project, project.tl());
    println!("audio: {clips} clip(s), {silent} at zero volume, muted track with sound: {muted}");
    println!(
        "video: {} frames composited on the GPU at {}x{}",
        project.tl().duration(),
        settings.width,
        settings.height
    );

    run_export_blocking(&tools, project, settings)?;
    let probed = ffmpeg::probe(&tools, std::path::Path::new(out))?;
    println!(
        "result: {}x{} {:.2}s video={} audio={}",
        probed.width, probed.height, probed.duration, probed.has_video, probed.has_audio
    );
    if !probed.has_audio {
        bail!("the exported file has no audio stream");
    }
    Ok(())
}

/// The point of building spans on demand is that a long recording costs nothing to open and that
/// jumping into the middle of it prepares only what you landed on. This uses a clip long enough to
/// span several segments and checks both.
fn on_demand_check(tools: &Arc<Tools>, dir: &std::path::Path) -> Result<()> {
    let long = dir.join("long.mp4");
    let secs = 30;
    let status = ffmpeg::command(&tools.ffmpeg)
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30"])
        .args(["-t", &secs.to_string(), "-c:v", "libx264", "-preset", "ultrafast"])
        .args(["-pix_fmt", "yuv420p", "-g", "60"])
        .arg(&long)
        .status()
        .context("building the long test clip")?;
    if !status.success() {
        bail!("could not build the long test clip");
    }

    let frames = secs * 30;
    let total_spans = ((frames + crate::proxy::SEG_FRAMES - 1) / crate::proxy::SEG_FRAMES) as u64;
    let builder = crate::proxy::ProxyBuilder::new(tools.clone(), 2);
    let media_id = 1u64;
    builder.register(crate::proxy::ProxySource {
        media: media_id,
        path: long.clone(),
        dir: dir.join("longspans"),
        fps: 30,
        height: 360,
        total_frames: frames,
    });
    let cache = FrameCache::new(64 * 1024 * 1024, 2, builder.clone());

    // Land near the end. Only the span under the playhead should be prepared.
    let target = (frames - 20) as u32;
    let t0 = Instant::now();
    let mut got = None;
    while Instant::now() < t0 + Duration::from_secs(90) {
        if let Some(f) = cache.get(media_id, target) {
            got = Some(f);
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    got.context("the span at the end never became available")?;
    let ms = t0.elapsed().as_millis();
    let built = builder.built.load(std::sync::atomic::Ordering::Relaxed);

    if built > 2 {
        bail!("landing on one point built {built} spans; it should build the one it landed in");
    }
    if total_spans < 4 {
        bail!("test clip is too short to exercise spanning ({total_spans} spans)");
    }
    // Seeking into a long file must not cost proportionally more than the start does.
    if ms > 15_000 {
        bail!("preparing a span {}s into the file took {ms} ms", secs);
    }
    pass!(
        "seeked {}s in: {built} of {total_spans} spans built, ready in {ms} ms",
        target / 30
    );
    std::fs::remove_file(&long).ok();
    Ok(())
}

/// Confirms the binary was actually built with a graphics backend.
///
/// This exists because it once was not: `default-features = false` on eframe left wgpu with no
/// backend compiled in, and the application aborted the instant it tried to open a window. Every
/// other check passed, because none of them open one. Checking the compiled feature set catches
/// that without needing a display.
fn gpu_backend_check() -> Result<()> {
    let backends = wgpu::Instance::enabled_backend_features();
    if backends.is_empty() {
        bail!(
            "no GPU backends are compiled into this binary — it would abort on launch with \
             \"No wgpu backend feature that is implemented for the target platform was enabled\". \
             Check the wgpu feature list in Cargo.toml."
        );
    }
    // Whether a usable adapter exists depends on the machine; a CI runner may legitimately have
    // none, so that is reported rather than failed on.
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends,
        ..Default::default()
    });
    let adapters = instance.enumerate_adapters(backends);
    let names: Vec<String> = adapters
        .iter()
        .map(|a| {
            let info = a.get_info();
            format!("{} ({:?})", info.name, info.backend)
        })
        .collect();
    if names.is_empty() {
        println!("  --  backends {backends:?} compiled in, but this machine exposes no adapter");
    } else {
        pass!("GPU backends {:?}, adapters: {}", backends, names.join(", "));
    }
    Ok(())
}

/// Mean colourfulness of one frame, used to tell a dissolve apart from a hard cut.
fn frame_chroma(tools: &Tools, file: &std::path::Path, at: f64) -> Result<f64> {
    let out = ffmpeg::command(&tools.ffmpeg)
        .args(["-v", "error", "-ss", &format!("{at:.3}"), "-i"])
        .arg(file)
        .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .context("sampling an exported frame")?;
    if !out.status.success() || out.stdout.is_empty() {
        bail!("could not sample a frame at {at}s");
    }
    let mut total = 0f64;
    let mut n = 0f64;
    for px in out.stdout.chunks_exact(3) {
        let hi = *px.iter().max().unwrap() as f64;
        let lo = *px.iter().min().unwrap() as f64;
        total += hi - lo;
        n += 1.0;
    }
    Ok(total / n.max(1.0))
}

/// Renders a greyscale clip dissolving into a colour one, then samples three frames from the
/// exported file. A successful export proves nothing on its own — if the dissolve were dropped,
/// the middle frame would match one side exactly. It has to sit between them.
fn dissolve_check(
    tools: &Arc<Tools>,
    dir: &std::path::Path,
    src: &std::path::Path,
    dur: f64,
) -> Result<()> {
    let mut project = Project::default();
    let media_id = project.alloc_id();
    project.media.push(media_item(media_id, src, dur));
    let tid = project
        .tracks()
        .iter()
        .filter(|t| t.kind == TrackKind::Video)
        .last()
        .map(|t| t.id)
        .context("no video track")?;

    let mut a = project.new_clip(ClipSource::Media(media_id), 0, 30, 0);
    a.color.saturation = 0.0; // fully grey
    let mut b = project.new_clip(ClipSource::Media(media_id), 30, 30, 60);
    b.transition_in = 20;
    project.track_mut(tid).unwrap().clips.push(a);
    project.track_mut(tid).unwrap().clips.push(b);
    project.normalize();

    let out = dir.join("dissolve.mp4");
    let settings = ExportSettings {
        path: out.clone(),
        encoder: Encoder::X264,
        quality: Quality::High,
        width: 320,
        height: 180,
        fps: 30,
            include_audio: true,
    };
    run_export_blocking(tools, project, settings)?;

    // frame 15 = grey only, frame 40 = halfway through the dissolve, frame 57 = colour only
    let grey = frame_chroma(tools, &out, 15.0 / 30.0)?;
    let mid = frame_chroma(tools, &out, 40.0 / 30.0)?;
    let colour = frame_chroma(tools, &out, 57.0 / 30.0)?;

    if colour <= grey + 5.0 {
        bail!("the colour test frames are not distinguishable (grey {grey:.1}, colour {colour:.1})");
    }
    if mid <= grey + 2.0 {
        bail!("mid-dissolve frame is still fully greyscale — the crossfade did not render");
    }
    if mid >= colour - 2.0 {
        bail!("mid-dissolve frame is already fully coloured — the crossfade did not render");
    }
    std::fs::remove_file(&out).ok();
    pass!("crossfade dissolves (chroma {grey:.1} → {mid:.1} → {colour:.1})");
    Ok(())
}

fn media_item(id: u64, src: &std::path::Path, dur: f64) -> MediaItem {
    MediaItem {
        id,
        path: src.to_path_buf(),
        name: "source.mp4".into(),
        duration: dur,
        frames: 120,
        src_width: 1280,
        src_height: 720,
        src_fps: 30.0,
        has_video: true,
        has_audio: true,
        audio_path: None,
        peaks_path: None,
        state: ImportState::Ready,
        error: None,
        bin: 0,
    }
}

/// A clip played at double speed should consume twice its timeline length in source and land at
/// exactly half the duration, with the audio retimed to match rather than dropped.
fn speed_check(
    tools: &Arc<Tools>,
    dir: &std::path::Path,
    src: &std::path::Path,
    dur: f64,
) -> Result<()> {
    let mut project = Project::default();
    let media_id = project.alloc_id();
    project.media.push(media_item(media_id, src, dur));
    let tid = project
        .tracks()
        .iter()
        .filter(|t| t.kind == TrackKind::Video)
        .last()
        .map(|t| t.id)
        .context("no video track")?;

    let mut fast = project.new_clip(ClipSource::Media(media_id), 0, 30, 0);
    fast.speed = 2.0;
    if fast.source_span() != 60 {
        bail!("a 30 frame clip at 2x should consume 60 source frames, got {}", fast.source_span());
    }
    if fast.source_frame(15) != 30 {
        bail!("frame mapping at 2x is wrong: {}", fast.source_frame(15));
    }
    project.track_mut(tid).unwrap().clips.push(fast);
    project.normalize();

    let out = dir.join("speed.mp4");
    let settings = ExportSettings {
        path: out.clone(),
        encoder: Encoder::X264,
        quality: Quality::Small,
        width: 640,
        height: 360,
        fps: 30,
            include_audio: true,
    };
    run_export_blocking(tools, project, settings)?;
    let probed = ffmpeg::probe(tools, &out)?;
    if (probed.duration - 1.0).abs() > 0.3 {
        bail!("2x clip exported as {:.2}s, expected 1.0s", probed.duration);
    }
    if !probed.has_audio {
        bail!("retimed clip lost its audio");
    }
    std::fs::remove_file(&out).ok();
    pass!("2x speed exported at {:.2}s with audio", probed.duration);
    Ok(())
}

fn run_export_blocking(
    tools: &Arc<Tools>,
    project: Project,
    settings: ExportSettings,
) -> Result<()> {
    let tl = project.tl().id;
    let job = export::start(tools.clone(), project, tl, settings);
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        if Instant::now() > deadline {
            bail!("export timed out");
        }
        match job.rx.recv_timeout(Duration::from_secs(10)) {
            Ok(ExportMsg::Done { .. }) => return Ok(()),
            Ok(ExportMsg::Failed(e)) => bail!("export failed: {e}"),
            Ok(_) => {}
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(e) => bail!("export channel closed: {e}"),
        }
    }
}

/// Exports a timeline with enough cuts that the filtergraph is far past the Windows command-line
/// limit, which is why it goes to ffmpeg as a script file rather than an argument.
fn big_graph_check(tools: &Arc<Tools>, dir: &std::path::Path, src: &std::path::Path, dur: f64) -> Result<()> {
    const CUTS: i64 = 120;
    let mut project = Project::default();
    let media_id = project.alloc_id();
    project.media.push(media_item(media_id, src, dur));
    let tid = project
        .tracks()
        .iter()
        .filter(|t| t.kind == TrackKind::Video)
        .last()
        .map(|t| t.id)
        .context("no video track")?;
    for i in 0..CUTS {
        let c = project.new_clip(ClipSource::Media(media_id), i * 5, 5, (i * 7) % 100);
        project.track_mut(tid).unwrap().clips.push(c);
    }
    project.normalize();

    let out = dir.join("big.mp4");
    let settings = ExportSettings {
        path: out.clone(),
        encoder: Encoder::X264,
        quality: Quality::Small,
        width: 640,
        height: 360,
        fps: 30,
            include_audio: true,
    };
    run_export_blocking(tools, project, settings)?;
    let probed = ffmpeg::probe(tools, &out)?;
    let expected = (CUTS * 5) as f64 / 30.0;
    if (probed.duration - expected).abs() > 0.5 {
        bail!("large export is {:.2}s, expected {expected:.2}s", probed.duration);
    }
    std::fs::remove_file(&out).ok();
    pass!("{CUTS}-cut timeline rendered");
    Ok(())
}

/// The timeline has to stay responsive on a feature-length edit, so check that the operations the
/// UI performs every frame do not degrade with clip count.
fn scale_check() -> Result<()> {
    const CLIPS: i64 = 10_000;
    let mut project = Project::default();
    let tid = project
        .tracks()
        .iter()
        .find(|t| t.kind == TrackKind::Video)
        .map(|t| t.id)
        .context("no video track")?;

    let t0 = Instant::now();
    for i in 0..CLIPS {
        let c = project.new_clip(ClipSource::Color([20, 20, 20, 255]), i * 30, 30, 0);
        project.track_mut(tid).unwrap().clips.push(c);
    }
    project.normalize();
    let build_ms = t0.elapsed().as_secs_f64() * 1000.0;

    if project.duration() != CLIPS * 30 {
        bail!("duration wrong after building {CLIPS} clips");
    }

    // clip_at is a binary search and runs for every track on every drawn frame.
    let track = project.track(tid).context("track vanished")?;
    let t0 = Instant::now();
    let mut found = 0u32;
    for i in 0..200_000i64 {
        let f = (i * 7919) % (CLIPS * 30);
        if track.clip_at(f).is_some() {
            found += 1;
        }
    }
    let lookup_ns = t0.elapsed().as_nanos() as f64 / 200_000.0;
    if found != 200_000 {
        bail!("clip_at missed {} lookups", 200_000 - found);
    }

    // A snapshot happens on every edit, so cloning the document must stay cheap.
    let t0 = Instant::now();
    let clone = project.clone();
    let clone_ms = t0.elapsed().as_secs_f64() * 1000.0;
    if clone.duration() != project.duration() {
        bail!("clone did not preserve the timeline");
    }

    if lookup_ns > 1000.0 {
        bail!("clip lookup took {lookup_ns:.0} ns, expected well under a microsecond");
    }
    pass!(
        "{CLIPS} clips: built in {build_ms:.0} ms, lookup {lookup_ns:.0} ns, undo snapshot {clone_ms:.1} ms"
    );
    Ok(())
}

/// A timeline that exercises everything phase A had to unify: a colour adjustment, a crossfade,
/// a title and a scaled, offset picture-in-picture.
fn parity_project(media_id: u64, src: &std::path::Path, dur: f64) -> Result<Project> {
    let mut project = Project::default();
    project.media.push(media_item(media_id, src, dur));
    // Three video tracks: the cut, the picture-in-picture, and the title above both.
    project.add_track(TrackKind::Video);

    let video: Vec<u64> = project
        .tracks()
        .iter()
        .filter(|t| t.kind == TrackKind::Video)
        .map(|t| t.id)
        .collect();
    let (top, mid, base) = (video[0], video[1], *video.last().context("no video track")?);

    let mut a = project.new_clip(ClipSource::Media(media_id), 0, 40, 0);
    a.color = crate::project::ColorAdjust { brightness: 0.06, contrast: 1.3, saturation: 0.55 };
    let mut b = project.new_clip(ClipSource::Media(media_id), 40, 40, 55);
    b.transition_in = 15;

    let mut pip = project.new_clip(ClipSource::Media(media_id), 10, 60, 20);
    pip.scale = 0.35;
    pip.pos_x = 0.28;
    pip.pos_y = -0.25;
    pip.opacity = 0.9;

    let title = project.new_clip(
        ClipSource::Text(TextProps {
            text: "Kite".into(),
            size: 0.22,
            ..Default::default()
        }),
        5,
        70,
        0,
    );

    project.track_mut(base).context("no base track")?.clips.push(a);
    project.track_mut(base).context("no base track")?.clips.push(b);
    project.track_mut(mid).context("no middle track")?.clips.push(pip);
    project.track_mut(top).context("no top track")?.clips.push(title);
    project.normalize();
    Ok(project)
}

/// Serves the render graph from the proxy cache, waiting for spans the way the preview does not
/// have to — a test can afford to block where the interface cannot.
struct ProxyFrames<'a> {
    cache: &'a FrameCache,
    missed: u32,
}

impl crate::render::FrameSource for ProxyFrames<'_> {
    fn frame(
        &mut self,
        _clip: u64,
        media: u64,
        src_frame: i64,
    ) -> Option<Arc<crate::decode::DecodedFrame>> {
        let f = src_frame.max(0) as u32;
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            if let Some(v) = self.cache.get(media, f) {
                return Some(v);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        self.missed += 1;
        None
    }
}

fn mean_abs_diff(a: &[u8], b: &[u8]) -> f64 {
    let n = a.len().min(b.len());
    if n == 0 {
        return 255.0;
    }
    let mut total = 0f64;
    // Alpha is opaque in both by construction; comparing it would only dilute the number.
    for i in 0..n {
        if i % 4 == 3 {
            continue;
        }
        total += (a[i] as f64 - b[i] as f64).abs();
    }
    total / (n as f64 * 0.75)
}

/// Pulls one exact frame out of a rendered file as RGBA.
fn exported_frame(tools: &Tools, file: &std::path::Path, n: i64, w: u32, h: u32) -> Result<Vec<u8>> {
    let out = ffmpeg::command(&tools.ffmpeg)
        .args(["-v", "error", "-i"])
        .arg(file)
        .args(["-vf", &format!("select=eq(n\\,{n})"), "-vsync", "0", "-frames:v", "1"])
        .args(["-f", "rawvideo", "-pix_fmt", "rgba", "-"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .context("reading a frame back out of the render")?;
    let want = (w * h * 4) as usize;
    if out.stdout.len() < want {
        bail!("frame {n} came back as {} bytes, expected {want}", out.stdout.len());
    }
    Ok(out.stdout[..want].to_vec())
}

/// **The exit criterion for phase A.**
///
/// The same timeline goes through both paths. The preview path plans the frame and renders it on
/// the GPU from proxy pictures; the export path plans the same frame, renders it on the GPU from
/// full-resolution decoders, and encodes it. Then the pixels are compared.
///
/// "Both finished" would prove nothing, so the tolerance is justified rather than picked: every
/// pair is also compared against a *different* frame of the same render, and the agreement has to
/// be far closer than that. If compositing ever drifts between the two paths, that margin closes.
fn parity_check(
    tools: &Arc<Tools>,
    dir: &std::path::Path,
    src: &std::path::Path,
    dur: f64,
    media_id: u64,
    cache: &Arc<FrameCache>,
) -> Result<()> {
    const W: u32 = 640;
    const H: u32 = 360;
    let project = parity_project(media_id, src, dur)?;
    if project.duration() != 80 {
        bail!("the parity timeline is {} frames, expected 80", project.duration());
    }

    let out = dir.join("parity.mp4");
    let settings = ExportSettings {
        path: out.clone(),
        encoder: Encoder::X264,
        quality: Quality::High,
        width: W,
        height: H,
        fps: 30,
        include_audio: false,
    };
    run_export_blocking(tools, project.clone(), settings)?;

    let gpu = crate::render::Gpu::headless().context("no GPU for the preview side of the check")?;
    let mut renderer = crate::render::Renderer::new(gpu)?;
    let mut source = ProxyFrames { cache, missed: 0 };

    // One frame per feature: the grade alone, the grade under a title and a picture-in-picture,
    // and the middle of the dissolve.
    let frames = [(3i64, "colour adjustment"), (25, "title and picture-in-picture"), (47, "crossfade")];
    let mut previews = Vec::new();
    let mut exports = Vec::new();
    let mut worst = 0f64;

    for (n, what) in frames {
        let plan = crate::render::plan_frame(project.tl(), n, W, H);
        renderer.render(&plan, &mut source)?;
        let preview = renderer.read_rgba()?;
        let exported = exported_frame(tools, &out, n, W, H)?;
        let d = mean_abs_diff(&preview, &exported);
        if d > 12.0 {
            bail!(
                "frame {n} ({what}) differs between the preview and the export by {d:.2} per \
                 channel — the two paths are not rendering the same picture"
            );
        }
        worst = worst.max(d);
        previews.push(preview);
        exports.push(exported);
    }

    if source.missed > 0 {
        bail!("{} proxy frames never became available", source.missed);
    }

    // The control. If the tolerance above were simply loose, this would pass too.
    let control = mean_abs_diff(&previews[0], &exports[1]);
    if control < worst * 3.0 {
        bail!(
            "the check cannot tell frames apart: matching frames differ by {worst:.2} and \
             different ones by only {control:.2}"
        );
    }

    // Each feature has to be visibly present in both renders, not merely equally absent.
    let (pw, ew) = (&previews[1], &exports[1]);
    for (name, px) in [("preview", pw), ("export", ew)] {
        let bright = px.chunks_exact(4).filter(|p| p[0] > 200 && p[1] > 200 && p[2] > 200).count();
        if bright < 300 {
            bail!("the title drew only {bright} bright pixels in the {name} render");
        }
    }
    // The picture-in-picture sits up and to the right of centre; that corner must differ from
    // the same spot on a frame where the picture-in-picture is not there.
    let corner = |px: &[u8]| {
        let (x, y) = ((W as f64 * 0.75) as usize, (H as f64 * 0.25) as usize);
        let i = (y * W as usize + x) * 4;
        [px[i], px[i + 1], px[i + 2]]
    };
    let with_pip = corner(&previews[1]);
    let without = corner(&previews[0]);
    if with_pip == without {
        bail!("the picture-in-picture did not change the frame where it should be");
    }

    std::fs::remove_file(&out).ok();
    pass!(
        "preview and export render the same pixels (worst {worst:.2} per channel, \
         unrelated frames {control:.1})"
    );
    Ok(())
}

/// Serves the mixer from a file on disk, the way the application serves it from what it has
/// already mapped.
struct FilePcm {
    media: u64,
    path: PathBuf,
    opened: Option<Arc<memmap2::Mmap>>,
}

impl crate::mix::PcmSource for FilePcm {
    fn pcm(&mut self, media: u64) -> Option<Arc<memmap2::Mmap>> {
        if media != self.media {
            return None;
        }
        if self.opened.is_none() {
            self.opened = crate::audio::open_pcm(&self.path);
        }
        self.opened.clone()
    }
}

/// Decodes a rendered file's sound back to the project's own format, so it can be compared with
/// what the mixer produced.
fn decode_audio_f32(tools: &Tools, file: &std::path::Path) -> Result<Vec<f32>> {
    let out = ffmpeg::command(&tools.ffmpeg)
        .args(["-v", "error", "-i"])
        .arg(file)
        .args(["-vn", "-f", "f32le", "-ac", "2", "-ar"])
        .arg(crate::project::SAMPLE_RATE.to_string())
        .arg("-")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .context("decoding the rendered sound")?;
    if !out.status.success() {
        bail!(
            "could not decode the rendered sound: {}",
            String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("unknown")
        );
    }
    Ok(out
        .stdout
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

/// Root mean square of the left channel over a range of frames, which is how loud something is.
fn rms(samples: &[f32], from: usize, frames: usize) -> f64 {
    let mut total = 0f64;
    let mut n = 0f64;
    for i in from..(from + frames) {
        if i * 2 >= samples.len() {
            break;
        }
        total += (samples[i * 2] as f64).powi(2);
        n += 1.0;
    }
    if n == 0.0 {
        return 0.0;
    }
    (total / n).sqrt()
}

/// Mean absolute difference per sample over a range of frames, with `b` shifted by `offset`.
fn sample_diff(a: &[f32], b: &[f32], from: usize, frames: usize, offset: i64) -> f64 {
    let mut total = 0f64;
    let mut n = 0f64;
    for i in from..(from + frames) {
        let j = i as i64 + offset;
        if j < 0 || i * 2 + 1 >= a.len() || (j as usize) * 2 + 1 >= b.len() {
            continue;
        }
        let j = j as usize;
        total += (a[i * 2] as f64 - b[j * 2] as f64).abs();
        total += (a[i * 2 + 1] as f64 - b[j * 2 + 1] as f64).abs();
        n += 2.0;
    }
    if n == 0.0 {
        return f64::MAX;
    }
    total / n
}

/// The dominant period of a signal, in samples, found by autocorrelation. Used to prove a retimed
/// clip still has the pitch it started with.
fn period_of(samples: &[f32], from: usize, frames: usize) -> i64 {
    let mut best = 0i64;
    let mut best_score = f32::NEG_INFINITY;
    for lag in 30..500i64 {
        let mut dot = 0f32;
        for j in 0..frames {
            let a = samples.get((from + j) * 2).copied().unwrap_or(0.0);
            let b = samples.get((from + j + lag as usize) * 2).copied().unwrap_or(0.0);
            dot += a * b;
        }
        if dot > best_score {
            best_score = dot;
            best = lag;
        }
    }
    best
}

/// A timeline that exercises everything the two mixers used to implement separately: two clips at
/// different volumes, a fade in and a fade out, an audio crossfade, a clip that does not start at
/// zero, and a clip whose speed is not 1.
fn audio_parity_project(media_id: u64, src: &std::path::Path, dur: f64) -> Result<Project> {
    let mut project = Project::default();
    project.media.push(media_item(media_id, src, dur));
    let tid = project
        .tracks()
        .iter()
        .filter(|t| t.kind == TrackKind::Video)
        .last()
        .map(|t| t.id)
        .context("no video track")?;

    // frames 0..50, loud, fading in from silence
    let mut a = project.new_clip(ClipSource::Media(media_id), 0, 50, 0);
    a.volume = 0.8;
    a.fade_in = 10;
    // frames 50..100, quiet, dissolving in over the tail of the first and fading out at its end.
    // The fade-out belongs to this clip rather than to `a`, or `a`'s own fade would reach silence
    // exactly where the dissolve starts and there would be nothing to cross.
    let mut b = project.new_clip(ClipSource::Media(media_id), 50, 50, 30);
    b.volume = 0.35;
    b.transition_in = 15;
    b.fade_out = 10;
    // frames 115..145, after a gap, at double speed
    let mut c = project.new_clip(ClipSource::Media(media_id), 115, 30, 0);
    c.speed = 2.0;
    c.volume = 0.9;

    project.track_mut(tid).context("no track")?.clips.push(a);
    project.track_mut(tid).context("no track")?.clips.push(b);
    project.track_mut(tid).context("no track")?.clips.push(c);
    project.normalize();
    Ok(project)
}

/// **The exit criterion for the mixer.**
///
/// The same timeline is mixed twice: once by the preview's mixer, straight out of `mix.rs`, and
/// once by a real export that encodes to AAC and is then decoded back. The samples are compared.
///
/// As with the picture, the tolerance is justified rather than picked. Every comparison is also
/// run against an *unrelated* stretch of the same render, and the check fails unless the matching
/// region is far closer than that — so quietly widening the tolerance breaks the control instead
/// of making the test pass.
fn audio_parity_check(
    tools: &Arc<Tools>,
    dir: &std::path::Path,
    src: &std::path::Path,
    dur: f64,
    media_id: u64,
    pcm_path: &std::path::Path,
) -> Result<()> {
    let project = audio_parity_project(media_id, src, dur)?;
    if project.duration() != 145 {
        bail!("the audio parity timeline is {} frames, expected 145", project.duration());
    }

    let out = dir.join("audio-parity.mp4");
    let settings = ExportSettings {
        path: out.clone(),
        encoder: Encoder::X264,
        quality: Quality::High,
        width: 320,
        height: 180,
        fps: 30,
        include_audio: true,
    };
    run_export_blocking(tools, project.clone(), settings)?;

    let mut source = FilePcm { media: media_id, path: pcm_path.to_path_buf(), opened: None };
    let plan =
        crate::mix::plan_audio(&project, project.tl(), &mut source, &mut Default::default());
    if plan.layers.len() != 3 {
        bail!("the plan has {} layers, expected 3", plan.layers.len());
    }
    let total = (145i64 * crate::project::SAMPLE_RATE as i64) / 30;
    let preview = plan.mix_range(0, total as usize);
    let exported = decode_audio_f32(tools, &out)?;
    if exported.len() < total as usize * 2 / 2 {
        bail!("the rendered file has only {} samples of sound", exported.len() / 2);
    }

    // AAC has an encoder delay, so find the alignment before judging the difference. A large
    // shift would itself be a fault, so the search is deliberately narrow.
    let f = |frames: i64| (frames * crate::project::SAMPLE_RATE as i64 / 30) as usize;
    let (probe_from, probe_len) = (f(20), f(20));
    let mut offset = 0i64;
    let mut best = f64::MAX;
    for k in -4096..=4096i64 {
        let d = sample_diff(&preview, &exported, probe_from, probe_len, k);
        if d < best {
            best = d;
            offset = k;
        }
    }

    // How loud the material actually is, so the numbers below are a fraction of the signal
    // rather than an absolute that only holds for one test file.
    let level = rms(&preview, f(25), f(15));
    if level < 0.01 {
        bail!("the parity mix is essentially silent ({level:.4}); the check would prove nothing");
    }

    // Compare across the whole timeline, not just the window the alignment was found on.
    let matched = sample_diff(&preview, &exported, f(2), f(140), offset);
    if matched > level * 0.03 {
        bail!(
            "the preview mix and the rendered sound differ by {matched:.4} per sample against a \
             signal level of {level:.4} — the two paths are not producing the same audio"
        );
    }

    // The control: the same preview samples against a stretch of the render they do not belong
    // to. If the tolerance above were simply loose, this would pass too.
    let control = sample_diff(&preview, &exported, f(10), f(20), offset + f(60) as i64);
    if control < matched * 8.0 {
        bail!(
            "the check cannot tell one part of the mix from another: matching audio differs by \
             {matched:.4} and unrelated audio by only {control:.4}"
        );
    }

    // Each thing the timeline was built to exercise has to be present in *both* mixes.
    let shift = |n: usize| (n as i64 + offset).max(0) as usize;
    for (name, mix, off) in [("preview", &preview, 0usize), ("export", &exported, shift(0))] {
        let one = rms(mix, off + f(25), f(15));
        let two = rms(mix, off + f(72), f(13));
        let head = rms(mix, off + f(1), f(2));
        let tail = rms(mix, off + f(98), f(2));
        let gap = rms(mix, off + f(103), f(8));
        let delayed = rms(mix, off + f(125), f(10));
        // Just inside the dissolve, where the outgoing clip is still nearly at full level. A hard
        // cut would already be down at the incoming clip's level here.
        let dissolve = rms(mix, off + f(51), f(2));

        if one < level * 0.5 {
            bail!("the {name} mix has no sound in the first clip ({one:.4})");
        }
        // 0.8 against 0.35 — the volumes have to survive as a ratio, not just as noise.
        let ratio = one / two.max(1e-9);
        if !(1.7..=3.0).contains(&ratio) {
            bail!("the {name} mix has the two clips at a level ratio of {ratio:.2}, expected about 2.3");
        }
        if head > one * 0.35 {
            bail!("the {name} mix does not fade in ({head:.4} against {one:.4})");
        }
        if gap > one * 0.05 {
            bail!("the {name} mix has sound in the gap between clips ({gap:.4})");
        }
        if delayed < one * 0.5 {
            bail!("the {name} mix lost the clip that starts late ({delayed:.4})");
        }
        if tail > two * 0.4 {
            bail!("the {name} mix does not fade out ({tail:.4} against {two:.4})");
        }
        if dissolve < two * 1.4 {
            bail!(
                "the {name} mix cuts rather than crosses into the second clip: {dissolve:.4} just \
                 inside the dissolve against {two:.4} after it"
            );
        }
        // The retimed clip must still be a 440 Hz tone — about 109 samples per cycle. Resampling
        // instead of stretching would halve that.
        let p = period_of(mix, off + f(125), 2000);
        if (p - 109).abs() > 6 {
            bail!(
                "the {name} mix has the retimed clip at a period of {p} samples, expected about \
                 109 — the speed change is not pitch preserving"
            );
        }
    }

    std::fs::remove_file(&out).ok();
    pass!(
        "preview and export mix the same samples (offset {offset}, matched {matched:.4} against a \
         {level:.3} signal, unrelated {control:.4})"
    );
    Ok(())
}

/// The interactive path has a 16 ms budget. This measures a full-resolution preview frame.
///
/// A CI runner with no graphics card falls back to a software rasteriser, where 16 ms is not a
/// meaningful target — so that case is measured and reported rather than failed on, and the gate
/// only bites where there is real hardware to gate.
fn preview_budget_check(
    cache: &Arc<FrameCache>,
    media_id: u64,
    src: &std::path::Path,
    dur: f64,
) -> Result<()> {
    let project = parity_project(media_id, src, dur)?;
    let gpu = crate::render::Gpu::headless()?;
    let software = {
        let a = gpu.adapter.to_lowercase();
        a.contains("llvmpipe") || a.contains("software") || a.contains("swiftshader") || a.contains("lavapipe")
    };
    let mut renderer = crate::render::Renderer::new(gpu)?;
    let mut source = ProxyFrames { cache, missed: 0 };

    // Warm the caches first; the budget is about steady-state playback, not the first frame.
    for n in [20i64, 21, 22] {
        let plan = crate::render::plan_frame(project.tl(), n, 1920, 1080);
        renderer.render(&plan, &mut source)?;
    }
    let t0 = Instant::now();
    const N: i64 = 20;
    for i in 0..N {
        let plan = crate::render::plan_frame(project.tl(), 20 + i, 1920, 1080);
        renderer.render(&plan, &mut source)?;
    }
    // Nothing is read back on the preview path, so wait for the queue rather than time a
    // submission that has not run yet.
    renderer.read_rgba()?;
    let ms = t0.elapsed().as_secs_f64() * 1000.0 / N as f64;

    if software {
        println!(
            "  --  preview frame {ms:.2} ms at 1920x1080 on a software rasteriser ({}); \
             the 16 ms gate needs real hardware",
            renderer.adapter()
        );
    } else if ms > 16.0 {
        bail!("a 1920x1080 preview frame took {ms:.2} ms, over the 16 ms interactive budget");
    } else {
        pass!("preview frame {ms:.2} ms at 1920x1080 on {}", renderer.adapter());
    }
    Ok(())
}

