# 2. Core Architecture

> **Status.** This document is the intended destination, not a description of what is built.
> As of phase A there is **one renderer** — a GPU graph in `src/render.rs` with fp16 targets and
> effects as shaders, shared by the preview and the export. The node graph of §2.2 and the kernel
> fusion of §2.3 do **not** exist yet; the render graph is the first step towards them, and the
> 4x fusion claim remains unmeasured. See [08-from-here.md](08-from-here.md).

## 2.1 Why the incumbents are slow

Design decisions are only defensible against the alternative. Every choice below is a response
to a specific, identifiable reason the existing tools struggle on modest hardware:

| Incumbent behaviour | Cost | Our answer |
|---|---|---|
| One full-resolution intermediate buffer allocated per effect/node | Memory bandwidth is the binding constraint on integrated GPUs; a 12-node grade means 12 round trips through VRAM | Kernel fusion (§2.3) — that grade becomes **one** dispatch |
| fp32 working precision everywhere by default | 2x the bandwidth of fp16 for imperceptible benefit in most operations | Precision policy per node (§2.6) |
| Whole-frame evaluation regardless of what is visible | Scrubbing a 4K frame in a 600px viewport computes ~45x the pixels needed | ROI + mip-level pull evaluation (§2.4) |
| Separate render paths per "page"/module (Edit vs Fusion vs Color) | Three caches, three schedulers, round trips at the seams | One graph, one evaluator (§2.2) |
| Long-GOP decode on the interactive path | A single H.264 seek can require decoding 250 frames | Mandatory HW decode + GOP index + background proxy (§2.7) |
| Plugin ABIs that hand plugins a CPU pixel buffer | Forces GPU→CPU→GPU per plugin; one legacy plugin can halve your frame rate | Declarative GPU effect format; CPU plugins isolated and out-of-band (§2.9) |

None of this is a criticism of the incumbents' engineering — most of it is the accumulated cost
of decades of backwards compatibility. Being new is our only structural advantage, and the
architecture should spend it aggressively.

## 2.2 One graph to rule them all

There is exactly one intermediate representation: a **directed acyclic graph of image, audio,
and geometry operations**, with time as a first-class input to every node.

The user-facing surfaces are *views* that compile down to it:

- **Timeline** — a temporal layout. A track stack of clips with transitions compiles into a
  graph of source → transform → blend nodes, generated on edit, cached, and diffed.
- **Node editor** — a direct view of a subgraph. This is what Fusion/Nuke users want.
- **Layer/comp view** — an After Effects–style stack, which is a graph with a linear-chain
  constraint and a different inspector. Same nodes underneath.
- **Simple mode** — a curated set of graph *templates* with a handful of exposed parameters.

This matters more than it sounds. In Resolve, moving between Edit, Fusion, and Color crosses
engine boundaries, which is why the seams are where the stutters and the render-cache
invalidations live. Here, a colour grade *is* a node in the same graph as the transition above
it, so the fusion compiler can merge them into a single kernel. **A user switching "pages" is
switching an inspector panel, not a render engine.**

```
Timeline view ─┐
Node view ─────┼──► Document (immutable) ──► Graph compiler ──► Render plan ──► Backend
Layer view ────┤                                    ▲                              │
Simple mode ───┘                                    └──── Cache (VRAM/RAM/disk) ◄──┘
```

## 2.3 The graph compiler: fusion is the whole game

Nodes do not execute. Nodes *emit operations* into a compilation unit, and the compiler
produces a render plan. Per frame request, the compiler runs these passes:

1. **Prune** — drop nodes that cannot affect the requested region (fully clipped, zero opacity,
   disabled, downstream of a solo).
2. **Constant-fold** — parameters not animated at this time collapse to literals; static
   subgraphs collapse to a cached texture handle.
3. **Fuse** — this is the one that matters. Chains of *pointwise* operations (colour ops,
   curves, LUT applications, blend modes, matte math, most CapCut-style "filters") are compiled
   into a **single generated compute kernel**. A 15-node grade becomes one dispatch, one read,
   one write.
4. **Schedule** — order tiles and dispatches to maximise cache residency; identify what can run
   ahead on other frames.
5. **Allocate** — assign transient buffers from a pool with liveness analysis, the way a
   modern render-graph does. Peak memory is a function of graph *width*, not node count.

Non-pointwise nodes (blur, warp, tracker, keyer with spatial support) break fusion groups; they
become boundaries, and we fuse aggressively on both sides. Separable and multi-pass operations
declare their own passes so the scheduler can still interleave them.

Generated kernels are cached on disk keyed by the fused-chain signature, so the shader compile
cost is paid once per user per unique effect chain, and we ship a warm cache of the common ones.

> **Phase 0 must prove this.** A benchmark harness comparing a 15-node fused grade against the
> same grade with per-node buffers, on integrated graphics, at 4K. If the win is not
> 4x or better, the central premise of this project is wrong and we should know in month two,
> not year two.

## 2.4 Pull-based, ROI-driven, tiled, multi-resolution evaluation

Evaluation is **pull-based**: a sink (viewport, scope, export writer) requests
`(region, resolution level, time)`. That request propagates *backwards* through the graph, each
node transforming the requested region into what it needs from its inputs.

Three multipliers stack here:

- **Region of interest.** Only visible tiles are computed. Panning a zoomed-in 8K plate touches
  only the tiles that entered the viewport.
- **Resolution level.** Every source and cache entry is a mip pyramid. Viewing at 50% zoom
  evaluates the half-res level — a **4x** reduction in pixels for free, and visually identical
  because it is being displayed at that size anyway.
- **Tiling.** Work is chunked (typically 256×256) so it parallelises cleanly, streams within a
  small VRAM budget, and lets us render visible tiles first and fill in the rest.

Combined, scrubbing a 4K timeline in a 720p viewport at 60% zoom computes roughly **1/12th** of
the pixels a naive full-frame evaluator does, before any other optimisation. This is the single
biggest reason the potato target is achievable.

Interactive quality degradation is explicit and tuned rather than accidental: during a scrub or
drag, drop one mip level and skip nodes marked `quality:refine`; on idle for ~120ms, re-render
at full quality. The user sees a smooth interaction that sharpens the instant they stop moving,
which reads as *fast* — far better than a correct-but-stuttering full-quality scrub.

## 2.5 Content-addressed cache

Every renderable result is keyed by a hash of: the fused subgraph structure, resolved parameter
values, input content hashes, time, resolution level, and colour space. That single decision
buys a startling number of features:

- Tweak a parameter and revert — the original is still cached, instantly.
- Undo/redo is instant for anything previously rendered.
- "Render in place" / background render is not a feature, it is the cache warming itself.
- Two clips with the same grade share cache entries.
- The cache survives restart, so reopening a project is instant.
- Renders on the farm and locally hit the same keys, so a shared cache means the export of a
  section already reviewed at full quality is a **file copy**.

Three tiers with cost-aware eviction (VRAM → system RAM → NVMe), where eviction weighs recompute
cost, not just recency: a cached tracker result or a 40-second denoise should outlive a cheap
colour tweak in the cache.

## 2.6 Precision policy

Working space is **scene-linear**, but the *storage* precision is per-node policy rather than a
global default:

- `fp16` default for image data. Half the bandwidth of fp32, and on integrated GPUs bandwidth is
  the wall we are hitting.
- `fp32` only where a node declares it needs it: accumulation-heavy operations, large-kernel
  convolutions, some spatial transforms, and anything where banding is demonstrable.
- `rgba8`/`rgb10` for pipeline segments that are provably display-referred and post-transform.

The compiler propagates precision requirements through fusion groups, so declaring fp32 on one
node widens only its group, not the whole graph. On typical iGPU workloads this is worth close
to 2x on its own.

## 2.7 Media I/O: the other half of the performance story

Compositing speed is irrelevant if the decoder cannot feed it. On low-end machines, decode is
usually the actual bottleneck and everyone blames "the effects".

- **Hardware decode is the primary path, not a fallback.** VideoToolbox, NVDEC, Quick Sync,
  VAAPI, MediaCodec. Decoded surfaces stay on the GPU and enter the graph as textures without
  ever touching system memory. A CPU decoder (dav1d, libavcodec) exists for unsupported formats
  and runs on background threads.
- **Index on import.** Build a GOP/keyframe index immediately (seconds, not minutes) so seeks
  are exact and cheap instead of requiring speculative decode. This is why some NLEs feel
  instant to scrub and others don't.
- **Automatic incremental proxies.** Long-GOP HEVC/H.264 is hostile to random access. On import,
  a background worker transcodes to a cheap all-intra proxy at a resolution matched to the
  machine's class, *lowest-numbered clips on the timeline first*, and the engine transparently
  uses whichever representation is ready. The user never manages this; it is not a checkbox in
  a submenu. Proxies live in the same content-addressed cache.
- **Frame-ahead pipelining.** Decode, process, and present run as separate lanes with a
  configurable lookahead so a single slow frame doesn't stall playback.
- **Smart passthrough on export.** Segments of the timeline that are unmodified and already in
  the delivery codec are copied at the bitstream level rather than re-encoded. On a long
  documentary with grading on 20% of shots, this is the difference between a 40-minute export
  and a 6-minute one.

## 2.8 Threading and the UI

- A **work-stealing job system** over a fixed thread pool sized to physical cores, with priority
  lanes: interactive (viewport) > lookahead > background (proxies, analysis, cache warming).
- **The UI thread never blocks on the engine.** Ever. The viewport shows the most recent
  available result and requests better ones; it does not wait. This is what makes an
  overloaded machine feel responsive instead of frozen.
- **The UI renders on its own GPU queue** at high priority with a tiny budget, so a saturated
  compositor cannot starve the interface. A UI that stays at 60fps while a render is pegging
  the machine is a large part of the perception of speed.
- Background work is **preemptible and yields**: importing 400 clips must not make trimming
  laggy.

## 2.9 Effects and plugins without the performance tax

The requirement is CapCut-scale effect breadth *and* Resolve-grade performance, which are
usually in tension because plugin ABIs are where speed goes to die.

Three tiers:

1. **Declarative effects** (the default, and what nearly everything ships as). An effect is a
   manifest: parameters, and shader source in our restricted kernel language. Because they are
   declarative, they **participate in fusion** — a stack of eight of them still compiles to one
   dispatch. Most CapCut-style filters, transitions, LUT looks, and text styles are this.
2. **Graph macros.** An effect built by *composing existing nodes*, with selected parameters
   exposed. Zero new code. This is how presets, templates, and most user-shared content work —
   and it means a user-made template is as fast as a built-in effect, because it *is* one.
3. **Native/OFX plugins** (escape hatch). Sandboxed in a separate process, explicitly marked in
   the UI as breaking fusion, and never on the interactive path by default. Compatibility
   matters for adoption; it must not set the performance ceiling.

Scripting (Python and/or Lua) drives the document and automation layer, not the per-pixel path.

## 2.10 Document model

Immutable persistent data structures plus an append-only command log:

- Undo/redo is a pointer move, not a state rebuild.
- Autosave is continuous and nearly free; crash recovery loses at most one command.
- Structural sharing means a 90-minute timeline with 10,000 clips costs little to snapshot, so
  versions and branches ("what if we tried this cut?") become cheap and normal.
- The command log is the natural substrate for real-time collaboration later (CRDT or OT on top)
  and for the tutorial system ([§5.6](05-feature-map.md)), which needs to drive and observe the same commands.
- Project files are a documented, diffable container — text-based structure with binary blobs
  side-loaded. Projects should survive the company.

## 2.11 Colour management

ACES-based, OCIO-compatible, scene-linear throughout, with the display transform applied only at
the end of the chain. Every input declares its transform (or is detected); every output declares
its target. HDR is the same pipeline with a different output transform, not a separate mode.

The beginner never sees this. In Simple mode, the entire system presents as "looks" and an
auto-detected input — but a beginner's project opened by a colourist is already correctly
managed, rather than being an unrecoverable mess of baked-in transforms.

## 2.12 3D, honestly

Full parity with Blender is out of scope (§1). What we build:

- **A real-time 3D layer inside the graph**: cameras, lights, materials, animation, driven by
  the same keyframe system as everything else.
- **USD as the interchange spine**, plus glTF and Alembic. Model in Blender, layout and render
  here, round-trip cleanly.
- **A hybrid renderer**: a rasterizer for interaction, progressive GPU path tracing for final
  quality, and a scalable fallback that keeps a *usable* preview on the potato.
- **Native AOV/deep compositing.** The 3D renderer outputs its passes directly into the graph
  with no intermediate files. Re-lighting, defocus from depth, and cryptomatte-based isolation
  happen live in the comp. This is the genuinely differentiated capability — it is what
  Fusion/AE users spend their lives working around — and it is worth far more than a modelling
  toolkit we would build worse than Blender does.

Modelling, sculpting, and physics simulation are revisited only after the core product is
established, and possibly never, in favour of deeper Blender interoperation.
