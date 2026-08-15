# 1. Vision and Scope

## The one-line thesis

**One node graph, compiled per frame, evaluated lazily at the resolution the viewer actually needs.**

Everything else in this plan follows from that sentence. The feature breadth of Resolve,
After Effects, Premiere, Blender and CapCut is achievable over time by a large team. What is
*not* achievable by adding features later is the engine architecture that makes them fast on a
laptop with integrated graphics. So the engine is the product, and the features are content
poured into it.

## Honest framing of the ask

The request is to match six flagship applications that collectively represent something on the
order of 3,000–6,000 engineer-years of work. Resolve alone has ~20 years of development.
Blender has ~30. No plan produces feature parity with all of them on any credible timeline, and
a plan that claims otherwise is not a plan.

What *is* credible, and what this document lays out:

1. A **core engine** that is genuinely 5–20x faster than the incumbents on low-end hardware,
   because they carry architectural debt that we get to skip. This is the whole bet.
2. A **capability roadmap** that reaches functional parity in the order that matters to users,
   with explicit "good enough to switch" bars rather than "matches feature list" bars.
3. Explicit **de-scoping** of the parts that are separate products in disguise — chiefly 3D
   modelling and simulation, which we consume via interchange rather than rebuild.

The differentiator is not the feature checklist. Every competitor already has the checklist.
The differentiator is that a 2017 ultrabook with Intel UHD graphics can scrub a graded 4K
timeline in real time. Nothing on the market does that.

## Who this is for

Three users, one document model, one engine. Not three products.

| User | Comes from | Needs | Wins because |
|---|---|---|---|
| **Beginner** | CapCut, phone editing | Templates, auto-captions, one-click looks, zero jargon | It opens in under 2 seconds and never stutters on their school laptop |
| **Working editor** | Premiere, Resolve Edit page | Long-timeline stability, media management, delivery | 90-minute timelines that don't degrade; scrubbing stays real time at hour two |
| **Artist** | After Effects, Fusion, Nuke | Node compositing, keying, tracking, expressions, 3D integration | Effects that compose without a render-preview wait cycle |

The progressive-disclosure design ([§5.6](05-feature-map.md)) is what lets one application serve all three without
becoming three applications sharing an installer.

## Non-goals

Stating these plainly, because scope creep is the primary failure mode for a project like this:

- **Not a 3D modelling or sculpting suite.** We import USD/glTF/Alembic, we light, shade,
  animate, and render in-context, and we composite AOVs natively. We do not build a modelling
  toolkit, a sculpt mode, a physics solver, or a geometry-nodes equivalent in the first three
  years. Blender is free and interoperates; the value we add is the *compositing integration*,
  not a second Blender.
- **Not a browser application in v1.** WebGPU is genuinely capable now, but the low-end target
  requires hardware video decode paths, direct file I/O, and memory control that the web
  sandbox costs us dearly. A web viewer/review tool is a Phase 4 item, not the primary product.
- **Not Electron, not a webview UI.** The UI must hold 60fps while the engine is saturated on
  a machine with 8GB of RAM. That rules out shipping a browser to draw a timeline.
- **Not a DAW.** Audio gets a real mixer, real metering, and clip-level effects. It does not get
  MIDI, virtual instruments, or a scoring workflow.
- **Not cloud-dependent.** Everything works offline. Collaboration and asset libraries are
  additive, never load-bearing.

## What "insanely fast" means, concretely

Vague performance goals produce vague performance. These are the numbers the engine is designed
against, and Phase 0 exists to prove them before any UI work begins. Full budget in
[03-performance-budget.md](03-performance-budget.md).

The reference low-end machine — **"the potato"** — is a 2018 laptop: 4-core CPU, Intel UHD 620
integrated graphics, 8GB RAM, SATA SSD. If it works there, it flies everywhere else.

- Cold start to editable timeline: **under 2 seconds**
- 1080p H.264 timeline with a 6-node grade, scrubbing at full frame rate on the potato
- 4K HEVC with the same grade at full frame rate on an M1 base / any discrete GPU
- Idle memory under **400MB**; working set on a 90-minute timeline under **2GB**
- Installer under **500MB**
- Timeline interaction (trim, ripple, drag) responds in **under 16ms at 10,000 clips**

These are engine-architecture numbers, not optimisation-pass numbers. You cannot reach them by
profiling your way out of a design that allocates a full-resolution intermediate buffer per
effect node.
