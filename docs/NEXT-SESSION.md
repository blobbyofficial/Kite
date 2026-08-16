# Brief for the next development session

Written at the end of the session that gave Kite one renderer, for whoever picks it up cold.

## What just happened

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

**Phase B: real media I/O.** Hardware decode (D3D11VA, NVDEC, Quick Sync) straight into GPU
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
| `src/project.rs` | Document model: bins, timelines, tracks, clips, render queue. Serde format — **migration matters** |
| `src/proxy.rs` | On-demand span building. A span is 150 frames, built when the timeline asks for a frame in it |
| `src/decode.rs` | Frame cache, prefetch, span lookup |
| `src/export.rs` | Per-clip full-resolution decoders, the audio filtergraph, and the encode pipe |
| `src/app.rs` | State, transport, editing commands, and `draw_preview`, which now just draws the render graph's texture |
| `src/timeline.rs`, `src/ui_chrome.rs` | Interface |
| `src/selftest.rs` | The end-to-end checks, including `parity_check` |

## The check that must never be weakened

`parity_check` in `src/selftest.rs` is the exit criterion for phase A and the thing that stops the
two paths drifting apart again. It renders one timeline — a colour adjustment, a crossfade, a
title and a scaled, offset picture-in-picture — through the preview path and through a real
export, and compares the pixels.

Its tolerance is *justified rather than picked*: every matched pair is also compared against a
different frame of the same render, and the check fails if matching frames are not several times
closer than unrelated ones. Currently matching frames agree to about 4 levels per channel and
unrelated ones differ by 20. If you find yourself widening that tolerance, something has drifted.

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
    stderr is read on its own thread for exactly this reason.

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

- A tester reported an export with no audio. It could not be reproduced. Diagnostics were added
  instead: the render queue states what sound will be included and why it might be none, and the
  finished file is probed and reported. **If it recurs, get the `.kite` file and the `--export`
  output.** Note that the audio path is unchanged by phase A — it is still an ffmpeg filtergraph.
- The frame-time budgets in [03-performance-budget.md](03-performance-budget.md) are still only
  measured on whatever machine happens to run the self-test. The reference-hardware CI fleet that
  document asks for does not exist.
- Kernel fusion, the central claim of [02-architecture.md](02-architecture.md), has never been
  measured. Phase D.
