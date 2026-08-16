# Brief for the next development session

Written at the end of the session that built the beta, for whoever picks it up cold.

## The job

**Phase A of [08-from-here.md](08-from-here.md): make Kite have one renderer.**

Right now the preview composites on the CPU through egui, and the export builds a giant ffmpeg
filtergraph that composites independently. Two renderers, kept in agreement by hand. Every effect
has to be written twice, and nothing structural stops them drifting. This blocks colour grading,
compositing, keyframes and everything else, so it goes first.

Build a GPU render graph on `wgpu` with offscreen fp16 targets and effects as shaders. The preview
draws from it. The export renders **the same graph**, frame by frame, and pipes raw frames to
ffmpeg for encoding only. ffmpeg keeps demux, decode and encode; it loses compositing.

**Do not add features in this phase.** Reaching parity with what already works, through one
renderer, is the whole deliverable.

## Exit criteria

A test that renders the same timeline through the preview path and the export path and compares
the pixels within tolerance. Not "both completed" — the same pixels. It must cover:

- a colour adjustment (brightness, contrast, saturation)
- a crossfade between two clips
- a title over a clip
- a scaled and offset picture-in-picture

Plus: everything in `kite --selftest` still passes, and the frame-time budget in
`docs/03-performance-budget.md` is not regressed.

## Orientation

- Repo: `blobbyofficial/Kite`, branch `main`. Build: `cargo build --release`.
- `cargo test` — unit tests, including project-format migration.
- `./target/release/kite --selftest` — the real one. Synthesises clips and exercises probe,
  on-demand span building, decode, titles, export, crossfade (by sampling exported pixels), speed,
  a 120-cut timeline, and a 10,000-clip model. **Run it after every change.**
- `./target/release/kite --export project.kite out.mp4` — headless render, prints the filtergraph.
- CI builds the Windows installer and publishes a release on every push to `main`.

Read `docs/02-architecture.md` for the original intent and `docs/08-from-here.md` for the honest
distance from it. The architecture document describes a unified node graph with kernel fusion.
**None of that exists.** Phase A is the first step toward it.

## Where things live

| File | What it is |
|---|---|
| `src/project.rs` | Document model: bins, timelines, tracks, clips, render queue. Serde format — **migration matters**, see below |
| `src/proxy.rs` | On-demand span building. A span is 150 frames, built when the timeline asks for a frame in it |
| `src/decode.rs` | Frame cache, prefetch, span lookup |
| `src/export.rs` | The ffmpeg filtergraph builder. **This is what phase A replaces** |
| `src/app.rs` | State, transport, editing commands, and `draw_preview` — the other renderer |
| `src/timeline.rs`, `src/ui_chrome.rs` | Interface |
| `src/selftest.rs` | The end-to-end checks |

## You can run the GUI headlessly. Do.

This was worth more than anything else last session — it found a bug that made the shipped
installer unable to start, and two layout bugs that were invisible from the code.

```bash
apt-get install -y xvfb mesa-vulkan-drivers libgl1-mesa-dri x11-utils imagemagick xdotool libxkbcommon-x11-0
Xvfb :99 -screen 0 1600x1000x24 &
export DISPLAY=:99
./target/release/kite path/to/project.kite &
sleep 15
import -window root shot.png          # then read the image
xdotool mousemove 900 585 click 1     # clicking works; synthetic key events do not, there is no WM
```

Build a `.kite` file by hand as JSON to set up a scenario — it is faster than driving the UI.

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
   probes which form works and remembers it. Bundled ffmpeg moves; do not assume a spelling.
6. **A filtergraph for a real edit exceeds the Windows command-line limit** — 120 cuts produced
   44 KB. It goes to ffmpeg in a file.
7. **ffmpeg with fontconfig silently substitutes a font when `fontfile` cannot be opened**, so a
   broken path looks like success. Render assets are staged in a directory that becomes ffmpeg's
   working directory and referenced by bare filename, so nothing needs escaping. There is a test
   that counts drawn pixels.
8. **"The export succeeded" proves nothing.** The crossfade test samples pixels from the exported
   file and asserts the middle of the dissolve sits between the two ends. Do this for anything
   visual.
9. **Several glyphs render as tofu** in the default egui fonts. Test any new symbol on screen.
10. **PowerShell does not block on a GUI-subsystem binary**, so `$LASTEXITCODE` is stale. CI uses
    `Start-Process -Wait -PassThru`.
11. **Renaming a serde field silently drops data.** Moving `tracks` into timelines needed
    `#[serde(rename = "tracks")]` on the legacy field or every saved project would have opened
    empty, with no error. Tests cover it. Any format change needs the same care.

## Unresolved

- A tester reported an export with no audio. It could not be reproduced — both a simple clip and a
  realistic split/gap/delayed edit exported at full level. Diagnostics were added instead: the
  render queue states what sound will be included and why it might be none, and the finished file
  is probed and reported. **If it recurs, get the `.kite` file and the `--export` output.**
- Kernel fusion, the central claim of `docs/02-architecture.md`, has never been measured. Phase D.
