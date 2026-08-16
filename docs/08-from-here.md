# 8. From here: Kite versus the nine

An honest measurement of the distance between what exists and what was asked for, and the order
the remaining work has to happen in.

## What exists, measured

Kite is **9,155 lines of Rust**. It cuts video competently: bins, several timelines per project,
trim, ripple, split, crossfade, speed, fades, titles, per-clip colour, a render queue, undo,
autosave. It scrubs quickly, and it prepares playback files only for the parts of a recording you
actually visit.

Three greps say more about the gap than any feature list:

| Question | Answer |
|---|---|
| Which module does GPU image processing? | **None.** `wgpu` appears once, in the self-test, checking a backend exists |
| Where is the animation system? | **There isn't one.** No keyframes, no interpolation, anywhere |
| Where is colour management? | **Nowhere.** 8-bit RGBA end to end, no working space |

Today a frame travels: ffmpeg decodes to MJPEG on the CPU → JPEG decode on the CPU → RGBA in
system memory → colour correction on the CPU → uploaded as a texture → drawn as a quad by the UI
toolkit. The graphics card composites the interface. It never touches the picture.

## The nine are not nine problems

They are four missing subsystems and a large amount of content poured into them.

| Missing subsystem | Which of the nine it unlocks |
|---|---|
| **An image pipeline** — GPU render graph, working space, effects as shaders | Resolve, After Effects, Fusion, most of CapCut |
| **An animation system** — keyframes, interpolation, expressions | After Effects, speed ramps, audio automation, grading over time |
| **An ML runtime** — GPU inference, model management | Topaz Video and Photo |
| **A 3D renderer** — scene, camera, shading, AOVs | Blender |

Premiere is mostly editing breadth Kite already has the shape of. Media Encoder and HandBrake are
mostly delivery breadth. yt-dlp is an afternoon. Those three are the cheap ones, and they are the
only cheap ones.

## The one thing blocking everything

**Kite has two renderers that must agree by hand.**

The preview composites on the CPU through the UI toolkit. The export builds a single enormous
ffmpeg filtergraph — 24 separate string concatenations — and ffmpeg composites it independently.
Nothing structural keeps them consistent.

This is already costing us. The colour controls had to be hand-matched to ffmpeg's `eq` filter by
replicating its luma-curve-and-chroma-scale decomposition, because that was the only way to make
what you see resemble what you get. The crossfade had to be verified by exporting a file and
sampling pixels out of it, because "the export succeeded" proved nothing about whether the
dissolve happened.

That approach does not survive contact with a node graph, keyframes, masks or tracking. Every
effect would have to be written twice, in two systems with different colour behaviour, different
alpha semantics and different timing, and kept in agreement forever.

**Everything else in this document depends on fixing this first.**

## Gap by application

Percentages are of the capability someone would need before they would switch, not of the feature
list.

| Application | Kite today | What is missing |
|---|---|---|
| **Premiere** long-form editing | ~30% | Three and four-point editing, insert/overwrite, slip/slide/roll, multicam, nesting, markers, sync by audio, relink and offline media, AAF/XML/EDL/OTIO, a real audio mixer with buses |
| **Resolve** colour | ~2% | Everything: node grading, wheels, curves, qualifiers, tracked windows, scopes, LUTs, ACES/OCIO, stills and versions, HDR. Also 10-bit, which 8-bit RGBA forecloses |
| **After Effects / Fusion** | ~2% | Keyframes and the graph editor, expressions, text animators, shape layers, masks and roto, planar and point tracking, camera solve, keying, particles, precomps, an effect SDK |
| **Blender** 3D | 0% | All of it. Scope-limited by design to a real-time layer plus USD interchange and AOV compositing |
| **Media Encoder / HandBrake** | ~25% | Containers and codecs beyond MP4/H.264, presets library, watch folders, batch CLI, two-pass, smart passthrough, deinterlace and filters, distributed rendering |
| **CapCut** approachability | ~5% | Templates, effects library, auto-captions, auto-reframe, beat detection, silence removal, background removal, tutorials |
| **yt-dlp** acquisition | 0% | URL import, format choice, playlists, subtitles and chapters. Genuinely small |
| **Topaz Video AI** | 0% | ML runtime, GPU inference, models for upscale, denoise, deblur, interpolation, stabilisation |
| **Topaz Photo AI** | 0% | Same runtime; arguably belongs as still-frame enhancement inside the editor rather than a second application |

## The order the work has to happen in

Each phase exists because the next one is unreasonable without it.

### A. One renderer *(unblocks everything)* — **done**

Built. `src/render.rs` holds a GPU render graph on `wgpu`: `Rgba16Float` offscreen targets, a
texture pool with mip chains, and every effect expressed as a shader. A `FramePlan` describes what
a timeline frame is made of; the preview renders it on the window's own device and hands egui a
texture, and the export renders **the same plan** frame by frame and pipes raw pixels to ffmpeg.
ffmpeg is now demux, decode, audio mixing and encode — it no longer composites anything.

The colour adjustment, the crossfade, the fades, the picture-in-picture transform and the titles
are each implemented once. Titles are rasterised with `ab_glyph` from the font egui already
embeds, so they no longer depend on a system TrueType file being openable.

Sound went the same way. `src/mix.rs` holds an `AudioPlan` and one mixer; the preview's cpal
callback and the export both execute it, and the export writes the result to a WAV that ffmpeg
encodes. `amix`, `afade`, `adelay`, `atempo`, `volume` and `alimiter` are gone, and with them the
last filtergraph — the export no longer passes ffmpeg a `-filter_complex` at all.

*Exit criteria, met:* two checks in `src/selftest.rs`, built the same way.

- `parity_check` renders one timeline — a colour adjustment, a crossfade, a title and a scaled,
  offset picture-in-picture — through both paths and compares the pixels. Matching frames agree to
  about 4 levels per channel (proxy resampling plus the H.264 round trip); unrelated frames differ
  by 20.
- `audio_parity_check` mixes one timeline — two clips at different volumes, a fade in, a fade out,
  a crossfade, a clip that starts late and a clip at double speed — through both paths and compares
  the samples. They agree to 0.0001 per sample against a 0.05 signal; unrelated audio differs by
  0.057, five hundred times further apart.

Both carry the same control, and both fail if that margin closes — so the tolerance cannot be
quietly widened to make a drift go away.

### B. Real media I/O

Hardware decode (D3D11VA, NVDEC, Quick Sync) straight into GPU textures. Keep the on-demand span
proxies only as the fallback for formats and machines that cannot. Establish a working colour
space and fp16 throughout.

*Exit criteria:* 1080p H.264 scrubs and plays with **no proxy at all** on the reference low-end
machine. This is the largest remaining "runs well on weak hardware" lever, and it also deletes
most of the waiting that still exists.

### C. Keyframes

The highest-leverage single feature in the entire list. One animation system gives motion
graphics, speed ramps, audio automation, animated masks and grading over time. Nothing above the
engine returns more per line written.

*Exit criteria:* any numeric parameter on any clip or effect can be animated, with a curve editor,
and it survives save, undo and export.

### D. The node graph and an effect SDK

With A and C done, colour grading and compositing stop being separate projects and become
collections of nodes. Build the graph, the effect format, and then the Resolve-style and
Fusion-style tool sets on top of it. This is where the original architecture plan's kernel fusion
belongs — and where it should finally be measured against the 4× claim that has never been tested.

### E. Acquisition and delivery breadth

yt-dlp integration, more containers and codecs, presets, watch folders, batch CLI, smart
passthrough. Cheap, visible, and independent of everything above — good work to interleave when a
harder phase stalls.

### F. ML enhancement

An ONNX or ncnn runtime, models as a node type in the graph from D. Upscale, denoise,
interpolation. See the honesty section below before promising anything here.

### G. The approachability layer

Templates as graph macros, auto-captions from on-device speech recognition, auto-reframe, beat
detection. All of it sits on D. Built earlier, it becomes the architecture and the professional
tools end up fighting it.

## "Shockingly fast on low-end devices", honestly

Some of what was asked for is in direct tension with the rest, and the plan is better for saying
so plainly.

**Compatible with the goal, and should stay uncompromised:** cutting, trimming, scrubbing,
playback, colour grading, compositing, titles. These are bandwidth and scheduling problems, and
the architecture plan's answers — fusion, region-of-interest evaluation, fp16, hardware decode —
are the right ones. A weak laptop can do all of this at frame rate.

**Not compatible, and should be sold as what it is:** ML enhancement and path-traced 3D. Topaz on
integrated graphics is minutes of compute per second of footage; no architecture changes that,
because the arithmetic is the cost. The honest shape is an opt-in, out-of-band render with a
small live preview region — never something that runs while you scrub.

This deserves to be a rule rather than a hope: **no feature may degrade the interactive path.**
Anything expensive is explicitly out-of-band, and the frame budget gates in
[03-performance-budget.md](03-performance-budget.md) stay build-breaking.

## Decisions worth making before any of this starts

1. **Does export keep using ffmpeg for encoding?** Recommended yes — encoders are not where the
   value is. But compositing must move out of it in phase A.
2. **Which ML runtime, and whose models?** The good upscaling and interpolation models are
   research-licensed or proprietary. This is a licensing question before it is a technical one,
   and it interacts with the PolyForm licence already chosen.
3. **How far does 3D go?** The existing plan scope-limits it to a real-time layer plus USD
   interchange and AOV compositing. That decision should be re-confirmed, not drifted past.
4. **Windows-only for how long?** The renderer in phase A is the moment portability gets cheap or
   expensive for years.
5. **Is the kernel fusion thesis still the bet?** It has never been measured. Phase D is where it
   gets tested, and the honest position is that it is unproven until then.
