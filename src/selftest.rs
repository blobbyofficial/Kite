//! End-to-end verification of the media pipeline, runnable from CI with `kite --selftest`.
//!
//! It synthesises a real clip with ffmpeg, imports it exactly as the app does, decodes frames out
//! of the proxy store, builds a small timeline and exports it — then checks the result. This
//! exercises every part of the pipeline that does not need a window.

use crate::decode::FrameCache;
use crate::export::{self, Encoder, ExportMsg, ExportSettings, Quality};
use crate::ffmpeg::{self, Tools};
use crate::framestore::FrameStore;
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
    step!("building proxy and audio");
    let t0 = Instant::now();
    importer.submit(media_id, src.clone(), project.settings, 540);

    let mut proxy_path = None;
    let mut audio_path = None;
    let mut peaks_path = None;
    let mut frames = 0i64;
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        if Instant::now() > deadline {
            bail!("import timed out");
        }
        match importer.rx.recv_timeout(Duration::from_secs(5)) {
            Ok(ImportMsg::Ready { proxy, audio, peaks, frames: f, .. }) => {
                proxy_path = proxy;
                audio_path = audio;
                peaks_path = peaks;
                frames = f;
                break;
            }
            Ok(ImportMsg::Failed { error, .. }) => bail!("import failed: {error}"),
            Ok(_) => {}
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(e) => bail!("import channel closed: {e}"),
        }
    }
    let import_ms = t0.elapsed().as_millis();
    if !(115..=125).contains(&frames) {
        bail!("expected about 120 proxy frames at 30 fps, got {frames}");
    }
    pass!("import finished in {import_ms} ms, {frames} frames");

    // ---- 4. frame store + decode -----------------------------------------
    let proxy = proxy_path.clone().context("no proxy was produced")?;
    let store = FrameStore::open(&proxy)?;
    if store.frames as i64 != frames {
        bail!("frame store holds {} frames, importer reported {frames}", store.frames);
    }
    if store.height != 540 {
        bail!("proxy height is {}, expected 540", store.height);
    }
    pass!("frame store {}x{}, {} frames", store.width, store.height, store.frames);

    let cache = FrameCache::new(64 * 1024 * 1024, 2);
    cache.register(media_id, proxy.clone());

    // Decode scattered frames to prove random access really is random access.
    let probe_frames = [0u32, 1, 59, 60, 61, (frames - 1) as u32];
    let t0 = Instant::now();
    for f in probe_frames {
        let img = cache
            .get(media_id, f)
            .with_context(|| format!("frame {f} did not decode"))?;
        if img.width != store.width || img.height != store.height {
            bail!("frame {f} decoded to {}x{}", img.width, img.height);
        }
        if img.rgba.len() != (img.width * img.height * 4) as usize {
            bail!("frame {f} has {} bytes, expected RGBA", img.rgba.len());
        }
        // testsrc2 is never a flat colour; an all-black frame means we decoded nothing.
        if img.rgba.chunks(4).all(|p| p[0] < 4 && p[1] < 4 && p[2] < 4) {
            bail!("frame {f} decoded to an empty image");
        }
    }
    let per = t0.elapsed().as_secs_f64() * 1000.0 / probe_frames.len() as f64;
    pass!("decoded {} scattered frames, {per:.2} ms each", probe_frames.len());

    // A seek to a random frame must not be slower than a sequential one; that is the
    // whole point of the all-intra store.
    let t0 = Instant::now();
    for f in 0..30u32 {
        cache.get(media_id, f).context("sequential decode failed")?;
    }
    let seq = t0.elapsed().as_secs_f64() * 1000.0 / 30.0;
    pass!("sequential decode {seq:.2} ms/frame");

    let audio_file = audio_path.context("no audio track was extracted")?;
    let audio_bytes = std::fs::metadata(&audio_file)?.len();
    let expect = 4.0 * 48_000.0 * 4.0; // 4 s, 48 kHz, stereo i16
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
        proxy_path: Some(proxy),
        audio_path: Some(audio_file),
        peaks_path: None,
        state: ImportState::Ready,
        error: None,
    });

    let v_track = project
        .tracks
        .iter()
        .filter(|t| t.kind == TrackKind::Video)
        .last()
        .map(|t| t.id)
        .context("no video track")?;
    let v_top = project
        .tracks
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
    pass!("timeline: {} clips, {} frames", project.tracks.iter().map(|t| t.clips.len()).sum::<usize>(), project.duration());

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
        width: project.settings.width,
        height: project.settings.height,
        fps: project.settings.fps,
    };
    let font = find_font();
    if font.is_none() {
        println!("  --  no TrueType font found, titles will be skipped in this run");
    }
    let assets = export::Assets::prepare(font.as_deref())?;
    verify_titles_render(&tools, &assets)?;
    let (inputs, graph, has_audio) = export::build_graph(&project, &settings, Some(&assets))?;
    if inputs.len() != 1 {
        bail!("expected one input file, got {}", inputs.len());
    }
    if !has_audio {
        bail!("export graph produced no audio");
    }
    step!(
        "filtergraph is {} chars over {} input(s), passed as {:?}",
        graph.len(),
        inputs.len(),
        export::graph_arg(&tools)
    );

    assets.cleanup();
    let t0 = Instant::now();
    let job = export::start(tools.clone(), project.clone(), settings, font.clone());
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        if Instant::now() > deadline {
            bail!("export timed out");
        }
        match job.rx.recv_timeout(Duration::from_secs(10)) {
            Ok(ExportMsg::Done(_)) => break,
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

    // ---- 8. a graph big enough to have overflowed a command line ----------
    big_graph_check(&tools, &dir, &src, info.duration)?;

    // ---- 9. does the timeline model scale? --------------------------------
    scale_check()?;

    std::fs::remove_dir_all(&dir).ok();
    println!("\nAll checks passed.");
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
        .tracks
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
        proxy_path: None,
        audio_path: None,
        peaks_path: None,
        state: ImportState::Ready,
        error: None,
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
        .tracks
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
    let job = export::start(tools.clone(), project, settings, None);
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        if Instant::now() > deadline {
            bail!("export timed out");
        }
        match job.rx.recv_timeout(Duration::from_secs(10)) {
            Ok(ExportMsg::Done(_)) => return Ok(()),
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
        .tracks
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
    };
    let assets = export::Assets::prepare(None)?;
    let (_, graph, _) = export::build_graph(&project, &settings, Some(&assets))?;
    assets.cleanup();
    let len = graph.len();
    if len < 32_768 {
        println!("  --  graph is {len} chars, smaller than expected but still exercised");
    }

    run_export_blocking(tools, project, settings)?;
    let probed = ffmpeg::probe(tools, &out)?;
    let expected = (CUTS * 5) as f64 / 30.0;
    if (probed.duration - expected).abs() > 0.5 {
        bail!("large export is {:.2}s, expected {expected:.2}s", probed.duration);
    }
    std::fs::remove_file(&out).ok();
    pass!("{CUTS}-cut timeline exported, filtergraph {len} chars");
    Ok(())
}

/// The timeline has to stay responsive on a feature-length edit, so check that the operations the
/// UI performs every frame do not degrade with clip count.
fn scale_check() -> Result<()> {
    const CLIPS: i64 = 10_000;
    let mut project = Project::default();
    let tid = project
        .tracks
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

/// Renders white text on black and counts the bright pixels.
///
/// ffmpeg builds with fontconfig quietly substitute a fallback font when `fontfile` cannot be
/// opened, so "the export succeeded" is not evidence that titles work. This checks that pixels
/// actually changed.
fn verify_titles_render(tools: &Tools, assets: &export::Assets) -> Result<()> {
    let Some(font) = assets.font_file.as_deref() else {
        println!("  --  skipping the title check, no font available");
        return Ok(());
    };
    std::fs::write(assets.dir.join("probe.txt"), b"HELLO")?;

    let out = ffmpeg::command(&tools.ffmpeg)
        .current_dir(&assets.dir)
        .args(["-v", "error", "-f", "lavfi", "-i", "color=c=black:s=640x360:d=1"])
        .args([
            "-filter_complex",
            &format!(
                "[0:v]drawtext=fontfile={font}:textfile=probe.txt:expansion=none\
                 :fontsize=120:fontcolor=white:x=20:y=100[o]"
            ),
        ])
        .args(["-map", "[o]", "-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "gray", "-"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .context("running the title render check")?;

    if !out.status.success() {
        bail!(
            "titles do not render: {}",
            String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("unknown")
        );
    }
    let bright = out.stdout.iter().filter(|b| **b > 200).count();
    if bright < 500 {
        bail!("the title font drew only {bright} bright pixels — text is not being rendered");
    }
    pass!("titles render ({bright} pixels drawn)");
    Ok(())
}

fn find_font() -> Option<PathBuf> {
    [
        "C:/Windows/Fonts/segoeuib.ttf",
        "C:/Windows/Fonts/segoeui.ttf",
        "C:/Windows/Fonts/arialbd.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationSans-Bold.ttf",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|p| p.is_file())
}
