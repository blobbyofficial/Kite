# Kite

A video editor built around one idea: **it should stay fast on a modest laptop.**

**[kite website](https://blobbyofficial.github.io/Kite/)** · **[Download](../../releases/latest)**

This repository holds two things — a working beta of the editor, and the architecture plan it is
the first step of.

---

## The beta

**[Download the installer from Releases](../../releases/latest)** — run `KiteSetup-*.exe` and you
can cut a video immediately. ffmpeg is bundled, so there is nothing else to set up. Every push
also leaves the installer as a build artifact on the [Actions tab](../../actions).

Start with [docs/BETA-README.md](docs/BETA-README.md).

**What it does today:** multi-track video and audio, trim and ripple editing, multi-clip drag
between tracks, snapping, rubber-band select, copy/paste, split at the playhead, crossfades,
speed from 0.25× to 4× with retimed audio, per-clip brightness/contrast/saturation, volume with
fades, opacity, scale and position, titles, waveforms and thumbnails on the timeline, audio
metering, undo/redo, autosave with crash recovery, project save/load, and H.264 export that uses
hardware encoders when the machine has them.

**What it doesn't, yet:** colour wheels, curves and scopes; keyframes; transitions other than
crossfade.

### How it stays fast

The bet is that most of an editor's sluggishness is decided by architecture, not by tuning.

- **Proxy-first, random access.** Long-GOP H.264 is hostile to scrubbing — one seek can mean
  decoding hundreds of frames. On import, each file is transcoded once into an indexed all-intra
  container, so any frame is an offset lookup plus one decode. Measured in CI on a Windows
  runner: **1.3 ms per frame, and seeking costs the same as playing forward.**
- **Nothing heavy on the UI thread.** Decoding, import and export all run on workers. The
  timeline shows the newest available frame rather than waiting for the right one, and prefetch
  keeps the region around the playhead warm.
- **Cost tracks what's on screen.** Clips, waveforms and thumbnails outside the visible window
  are never touched. Verified in CI: **10,000 clips build in ~1 ms, clip lookup is ~45 ns, and
  an undo snapshot of the whole document is ~0.5 ms.**
- **Audio is the master clock.** The output callback never allocates and never blocks, so audio
  stays continuous even when the picture cannot keep up.
- **Proxies are for editing only.** Export reads the original files, so the master is full
  quality regardless of what you scrubbed against.

### Verifying it yourself

```
kite --selftest
```

Synthesises a clip, imports it, decodes scattered frames, builds a timeline, checks that titles
actually render, exports, re-probes the result, and exercises a 120-cut timeline plus a
10,000-clip model. CI runs this on Windows before it will build an installer.

### Building

```
cargo build --release
```

Needs `ffmpeg` and `ffprobe` on `PATH` or in an `ffmpeg` folder beside the binary. The installer
is built by [.github/workflows/windows.yml](.github/workflows/windows.yml).

Licensed under [PolyForm Small Business 1.0.0](LICENSE.md) — free for individuals, creators and
small companies; see [LICENSING.md](LICENSING.md) for what that means in practice, including that
anything you make with Kite is entirely yours.

See [THIRD-PARTY.md](THIRD-PARTY.md) — the bundled ffmpeg is a GPL build, which needs a decision
before any public release.

---

## The plan

Where this is going: an editor with the combined capability surface of DaVinci Resolve, After
Effects, Premiere, Media Encoder, CapCut and Blender, with performance on low-end hardware as the
organising constraint rather than an afterthought.

> One node graph, compiled per frame, evaluated lazily at the resolution the viewer actually
> needs.

| | |
|---|---|
| [1. Vision and Scope](docs/01-vision-and-scope.md) | The thesis, an honest read of the ask, users, non-goals |
| [2. Core Architecture](docs/02-architecture.md) | The graph, kernel fusion, evaluation, cache, media I/O, plugins |
| [3. Performance Budget](docs/03-performance-budget.md) | Hard numbers, reference machines, CI gates, degradation |
| [4. Technology Choices](docs/04-tech-stack.md) | Rust core, custom RHI, custom UI toolkit, and why |
| [5. Feature Map](docs/05-feature-map.md) | How each reference app lands, with "good enough to switch" bars |
| [6. Roadmap](docs/06-roadmap.md) | Five phases, gated on a Phase 0 that can cheaply fail |
| [7. Risks](docs/07-risks.md) | What kills this, ranked, plus open questions |

One-time GitHub settings that automation cannot apply are listed in
[docs/REPO-SETUP.md](docs/REPO-SETUP.md).

The beta is a slice of Phase 1. The plan's central bet — compiling a node graph so a long chain
of effects becomes a single GPU dispatch — is not in it yet; that is Phase 0 work, and the
document explains why it should be proven before anything is built on top of it.
