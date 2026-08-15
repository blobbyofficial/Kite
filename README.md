# Video Editing App — Architecture Plan

A plan for a video editing application with the combined capability surface of DaVinci Resolve,
After Effects, Premiere Pro, Media Encoder/HandBrake, CapCut, and Blender — designed from the
first line around one constraint: **it must run beautifully on low-end hardware.**

## The thesis

> One node graph, compiled per frame, evaluated lazily at the resolution the viewer actually
> needs.

The incumbents are slow on modest machines for identifiable architectural reasons — a
full-resolution buffer per effect node, fp32 everywhere by default, whole-frame evaluation, and
separate render paths per module. Being new is our one structural advantage, and this design
spends it aggressively:

- **Kernel fusion** — a 15-node colour grade compiles to *one* GPU dispatch, not 15 round trips
  through VRAM. Memory bandwidth is the wall on integrated graphics.
- **ROI + mip + tiled evaluation** — scrubbing a 4K timeline in a windowed viewport computes
  roughly a twelfth of the pixels a naive evaluator does.
- **Content-addressed caching** — instant undo, free background render, cache that survives
  restarts and shares with the render farm.
- **fp16 by default**, fp32 only where a node proves it needs it.
- **Hardware decode as the primary path**, with automatic incremental proxies.

The reference target — "the potato" — is a 2018 ultrabook with Intel UHD 620 and 8GB of RAM.
1080p with a six-node grade must scrub in real time there.

## Documents

| | |
|---|---|
| [1. Vision and Scope](docs/01-vision-and-scope.md) | The thesis, an honest read of the ask, users, non-goals |
| [2. Core Architecture](docs/02-architecture.md) | The graph, the compiler, evaluation, cache, media I/O, plugins |
| [3. Performance Budget](docs/03-performance-budget.md) | Hard numbers, reference machines, CI gates, degradation |
| [4. Technology Choices](docs/04-tech-stack.md) | Rust core, custom RHI, custom UI toolkit, and why |
| [5. Feature Map](docs/05-feature-map.md) | How each reference app lands, with "good enough to switch" bars |
| [6. Roadmap](docs/06-roadmap.md) | Five phases, gated on a Phase 0 that can cheaply fail |
| [7. Risks](docs/07-risks.md) | What kills this, ranked, plus open questions |

## Where to start

**[Phase 0](docs/06-roadmap.md#phase-0--prove-the-thesis-months-04-6-people)** — four months, six
engineers, no UI. Build the benchmark harness first, then the graph compiler, then answer one
question: on integrated graphics, is a fused 15-node grade at 4K genuinely ≥4x faster than the
per-node-buffer approach?

If yes, everything else in this plan is engineering. If no, the premise is wrong and we have
spent four months instead of four years finding out.
