# Brief for the next development session

Written at the end of the session that finished the unification — one renderer, then one mixer —
for whoever picks it up cold.

## What just happened, part two: one mixer

Sound had the same problem the picture had. The preview mixed i16 PCM in Rust through cpal; the
export emitted `amix`, `afade`, `adelay`, `atempo`, `volume` and `alimiter` and let ffmpeg do it.
Two mixers kept in agreement by hand — and they were not in agreement: the preview ignored clip
speed and dissolves entirely, and only the export applied a limiter.

`src/mix.rs` is now the mixer, built to the same shape as `render.rs`:

- `AudioPlan` — what should be heard, as data: layers with a source, a position in **samples**, a
  volume, fades, and dissolve ramps either side. `plan_audio` is the only place any of that is
  decided, and it mirrors `render::plan_frame` including the dissolve rule.
- `AudioPlan::mix_into` — the mixer. One gain envelope, one limiter, no allocation.
- `PcmSource` — the only thing that differs between the paths, and barely: both read the 48 kHz
  stereo PCM the importer extracts, so the two paths are literally mixing the same bytes.

The preview's cpal callback now only gets samples to the device. The export mixes the whole
timeline to a float WAV and hands it to ffmpeg as a second input. **There is no filtergraph left
anywhere** — the export passes ffmpeg no `-filter_complex` at all, which also retires the
ffmpeg-8 `-filter_complex_script` trap.

Things that were decided once, having previously been decided twice or not at all:

- **The limiter.** A smooth saturation to a 0.97 ceiling, applied in the mixer. It is a
  waveshaper, not a look-ahead limiter, which is what makes it sample-exact and therefore
  comparable between the paths. Previously the export had `alimiter=limit=0.97` and the preview
  hard-clamped at 1.0.
- **Clip edges.** A 2 ms ramp at every clip boundary. Audio cannot be hard-cut without clicking,
  and the frame grid does not care where the waveform is.
- **Speed.** WSOLA, so a clip at 2x keeps its pitch, as `atempo` used to give the export and the
  preview did not give at all. It is deterministic — which is the property that lets the two paths
  agree exactly — and it runs when the plan is built, cached in a `RetimeCache`, never in the
  audio callback.

**Worth knowing:** the export now mixes from the importer's 48 kHz stereo extraction rather than
decoding the original file's audio afresh. That is what makes sample-level parity possible at all.
It costs nothing audible — the output is 48 kHz AAC either way, and the old path resampled to
48 kHz too — but a 96 kHz / 24-bit source no longer reaches the mix at its own rate and depth. If
that ever matters, the fix is to raise the importer's extraction format, not to give the export a
second path.

## What happened before that: one renderer

**Phase A of [08-from-here.md](08-from-here.md) is done: Kite has one renderer.**

Before, the preview composited on the CPU through egui and the export built a 24-concatenation
ffmpeg filtergraph that composited independently. Every effect existed twice and nothing
structural kept the two in agreement.

Now there is `src/render.rs`. It holds a GPU render graph on `wgpu`:

- `FramePlan` — what a timeline frame is made of, as data: an ordered list of layers, each with a
  source (a media picture, a solid, or a title), an alpha, a transform and a colour adjustment.
  `plan_frame` builds it from a `Timeline`, and it is the *only* place compositing order, fades
  and dissolves are decided.
- `Renderer` — executes a plan on `Rgba16Float` offscreen targets. One shader does colour
  adjustment, tint and alpha; blending is premultiplied "over". Uploaded pictures get mip chains,
  which is what stops a scaled-down picture-in-picture aliasing.
- `FrameSource` — the only thing that differs between the two paths. The preview answers from the
  proxy cache; the export answers from one full-resolution ffmpeg decoder per clip.

The preview renders on the window's own wgpu device and hands egui a texture, so a composited
frame never leaves the GPU. The export renders the same plan frame by frame, reads it back, and
pipes raw RGBA to ffmpeg on stdin. **ffmpeg no longer composites anything** — it demuxes, decodes,
mixes audio and encodes.

Titles are rasterised by us, with `ab_glyph`, from the font egui already embeds. They no longer
depend on a system TrueType file being present and openable.

## The job now

**Phase B: real media I/O**, and the working colour space that phase A deliberately left alone. Hardware decode (D3D11VA, NVDEC, Quick Sync) straight into GPU
textures, keeping the on-demand span proxies only as the fallback. Establish a working colour
space and carry fp16 through properly.

*Exit criteria:* 1080p H.264 scrubs and plays with **no proxy at all** on the reference low-end
machine.

Two things phase A deliberately left alone, which phase B should pick up:

1. **Colour is still gamma-space 8-bit in, gamma-space out.** The shader does its maths on
   gamma-encoded 0..1 values because that is what Kite's colour controls were built against, and
   phase A was about having one renderer, not about changing what a grade looks like. The single
   conversion point is `fs_display` in `render.rs`. A real working space belongs here.
2. **Export decoders are one ffmpeg process per clip**, reading raw RGBA over a pipe. Correct and
   sequential, but it is a full-resolution CPU decode and a memory copy per frame. Hardware decode
   into a texture replaces this whole path.

## Orientation

- Repo: `blobbyofficial/Kite`, branch `main`. Build: `cargo build --release`.
- `cargo test` — unit tests, including project-format migration and render-plan tests.
- `./target/release/kite --selftest` — the real one. **Run it after every change.**
- `./target/release/kite --export project.kite out.mp4` — headless render.
- CI builds the Windows installer and publishes a release on every push to `main`.

## Where things live

| File | What it is |
|---|---|
| `src/render.rs` | **The renderer.** Frame plans, the wgpu graph, shaders, title rasterisation |
| `src/mix.rs` | **The mixer.** Audio plans, the gain envelope, the limiter, WSOLA retiming, WAV output |
| `src/project.rs` | Document model: bins, timelines, tracks, clips, render queue. Serde format — **migration matters** |
| `src/proxy.rs` | On-demand span building. A span is 150 frames, built when the timeline asks for a frame in it |
| `src/decode.rs` | Frame cache, prefetch, span lookup |
| `src/export.rs` | Per-clip full-resolution decoders, PCM extraction, and the encode pipe |
| `src/app.rs` | State, transport, editing commands, and `draw_preview`, which now just draws the render graph's texture |
| `src/audio.rs` | The preview's audio sink and the transport clock. No mixing happens here any more |
| `src/timeline.rs`, `src/ui_chrome.rs` | Interface |
| `src/selftest.rs` | The end-to-end checks, including `parity_check` |

## The two checks that must never be weakened

`parity_check` and `audio_parity_check` in `src/selftest.rs` are what stop the paths drifting apart
again. Each renders or mixes one timeline through the preview path and through a real export, and
compares the output.

Both tolerances are *justified rather than picked*: every matched comparison is also run against an
unrelated stretch of the same render, and the check fails unless the matching one is several times
closer. Currently:

| | matched | unrelated | margin |
|---|---|---|---|
| Picture | 4.05 per channel | 20.1 | 5x |
| Sound | 0.0001 per sample | 0.0574 | 574x |

If you find yourself widening either tolerance, something has drifted. Widening it far enough to
pass breaks the control instead.

The audio check also asserts, in *both* mixes, that the volumes survive as a ratio, that the fades
fade, that the gap is silent, that the late clip arrives, that the dissolve crosses rather than
cuts, and that the retimed clip is still 440 Hz. "Both produced a file" would prove none of it.

## Traps this codebase has already fallen into

Every one of these cost real time. They are fixed; do not reintroduce them.

1. **`default-features = false` on eframe strips every wgpu graphics backend.** The app aborted on
   launch with "No wgpu backend feature ... was enabled", and CI was green because nothing in the
   self-test opens a window. `wgpu` is now a direct dependency with `vulkan, dx12, gles, metal`.
   The self-test checks `Instance::enabled_backend_features()` is non-empty. Keep that check.
2. **`Cargo.lock` is load-bearing.** `gpu-allocator` accepts `windows ">=0.53,<=0.58"` and Cargo
   will unify it onto the 0.54 that `cpal` pulls in, while `wgpu-hal` needs 0.58. Legal, and it
   only fails when dx12 compiles — so it looks fine on Linux and breaks on Windows. Cross-check
   with `cargo check --target x86_64-pc-windows-gnu` before trusting a dependency change.
3. **egui stores a panel's content rect and reuses it as next frame's height.** If you paint
   without allocating, the panel shrinks to nothing and stays there. `ui.allocate_rect` after
   painting.
4. **A right-to-left sub-layout claims all remaining width.** Anything centred must get its own
   reserved region first.
5. **ffmpeg 8 removed `-filter_complex_script`**, replaced by `-/filter_complex file`. The build
   probes which form works and remembers it. The audio graph still uses it. Do not assume a
   spelling.
6. **A filtergraph for a real edit exceeds the Windows command-line limit.** Even the audio-only
   graph reaches 19 KB on a 120-cut timeline. It goes to ffmpeg in a file.
7. **"The export succeeded" proves nothing.** Sample pixels. The crossfade check and the parity
   check both do.
8. **Several glyphs render as tofu** in the default egui fonts. Test any new symbol on screen.
9. **PowerShell does not block on a GUI-subsystem binary**, so `$LASTEXITCODE` is stale. CI uses
   `Start-Process -Wait -PassThru`.
10. **Renaming a serde field silently drops data.** Moving `tracks` into timelines needed
    `#[serde(rename = "tracks")]` on the legacy field or every saved project would have opened
    empty, with no error. Any format change needs the same care.
11. **`egui_wgpu::RenderState` hands out a `Device` and a `Queue` by value, not `Arc`s.** They are
    cheap handle clones, so wrapping them in an `Arc` for `render::Gpu` is right, but do not
    assume the field types.
12. **An sRGB render target re-encodes whatever the shader writes.** The renderer works in
    gamma-encoded values, so `fs_display` converts them *back* to linear before writing to the
    sRGB texture egui samples. Skip that and the preview comes out visibly washed out while the
    export stays correct — which looks like a preview bug and is not.
13. **Writing raw frames to ffmpeg's stdin deadlocks if its stderr is not drained.** The encoder's
    stderr is read on its own thread for exactly this reason. Raw *audio* deliberately does not go
    down a second pipe for the same reason — it is mixed to a WAV first and passed as a file.
14. **Clip positions are frames, audio is samples.** Always convert with
    `VideoSettings::frame_to_sample`, which is exact integer arithmetic. Anything that goes via
    seconds and floats will drift, and the drift shows up as audio slipping against picture on
    long timelines.
15. **Building the audio plan is not free.** A retimed clip is stretched while the plan is built,
    on the UI thread, and the plan is rebuilt on every edit. `RetimeCache` is what keeps that off
    the interactive path — five seconds of retimed audio costs about 50 ms to stretch, and a unit
    test gates it. Do not remove the cache.
16. **`sine` from lavfi is much quieter than full scale** — about 0.06 RMS after an AAC round
    trip. Audio checks written against absolute levels will fail for no reason; make them
    relative to a measured signal level, as `audio_parity_check` does.

## You can run the GUI headlessly. Do.

This keeps finding bugs that are invisible from the code, including one that made the shipped
installer unable to start.

```bash
apt-get install -y xvfb mesa-vulkan-drivers libgl1-mesa-dri x11-utils imagemagick xdotool \
  libxkbcommon-x11-0 libasound2-dev ffmpeg pkg-config
Xvfb :99 -screen 0 1600x1000x24 &
export DISPLAY=:99
./target/release/kite path/to/project.kite &
sleep 15
import -window root shot.png          # then read the image
xdotool mousemove 900 585 click 1     # clicking works; synthetic key events do not, there is no WM
```

Build a `.kite` file by hand as JSON to set up a scenario — it is faster than driving the UI. The
software Vulkan driver (llvmpipe) runs the render graph fine; it is just slow, which is why the
preview frame-time gate only bites on real hardware.

## Unresolved

- A tester reported an export with no audio, and it could never be reproduced. The filtergraph
  that was the most likely home for it no longer exists, and the render now fails loudly rather
  than quietly if the timeline had audio clips and the finished file has no sound track
  (`check_audio_arrived` in `export.rs`, unit tested). That class of bug should be gone; if
  something like it recurs, the error message will now say so instead of producing a silent file.
- The frame-time budgets in [03-performance-budget.md](03-performance-budget.md) are still only
  measured on whatever machine happens to run the self-test. The reference-hardware CI fleet that
  document asks for does not exist.
- Kernel fusion, the central claim of [02-architecture.md](02-architecture.md), has never been
  measured. Phase D.
