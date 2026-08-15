# 4. Technology Choices

Each choice below is justified against the low-end performance goal, because that is the
constraint that should decide every ambiguous call.

## Core engine — Rust

Memory safety without a garbage collector matters here specifically because **GC pauses are
frame drops**. A managed-language core would put an unpredictable multi-millisecond hitch in a
16ms budget. Rust also gives us fearless parallelism for the job system (essential when the
whole design is concurrent), first-class SIMD, and a good FFI story for the C/C++ libraries we
must use. Cost: a smaller hiring pool and slower ramp-up for GPU-graphics engineers who live in
C++. Accepted — the core is small and long-lived; the breadth lives in effects and UI.

C++ is used where the ecosystem is: codecs, OCIO, OFX host, some vendor SDKs, behind thin safe
wrappers.

## GPU — thin RHI over Vulkan / Metal / D3D12

A minimal in-house render hardware interface rather than a large third-party engine, because the
render-graph and allocation strategy in §2.3 *is* our product and it needs to be ours. `wgpu` is
the fallback plan and the likely choice for tooling and the eventual web viewer.

Two hard constraints from the potato target:

- **A compute-only path is mandatory.** Everything expressible as compute dispatches, so we are
  not at the mercy of fixed-function pipeline quirks on old integrated drivers.
- **A SIMD CPU backend must exist** and be genuinely usable (AVX2/NEON), for machines where the
  GPU driver is broken, blacklisted, or absent. It will be slower. It must not be unusable.

## UI — custom GPU-drawn toolkit

Not Electron, not a webview, not Qt widgets. The reasoning is in §2.8: the UI must hold 60fps
while the engine saturates the machine, and it must not add 150MB and 300ms of startup.

A retained-mode, GPU-composited toolkit rendering through the same RHI, with its own high
priority queue. Text shaping via HarfBuzz + a proper font stack; full accessibility bridges to
platform APIs from day one, not retrofitted.

This is real work — probably 8–12 months of a small dedicated team — and it is where projects
like this usually compromise and then regret it permanently. Budget for it honestly.

## Media

| Concern | Choice |
|---|---|
| Demux/container | libavformat, with our own index layer |
| Decode | Platform HW APIs first; libavcodec + dav1d fallback |
| Encode | Platform HW encoders; x264/x265/SVT-AV1 for quality tiers |
| Colour | OpenColorIO, ACES config |
| Image formats | OpenEXR, libraw for camera raw |
| 3D interchange | OpenUSD, glTF, Alembic |
| Audio | Own graph; platform APIs (CoreAudio/WASAPI/ALSA) |

## Scripting and extensibility

Python for automation and pipeline integration (it is what the industry already writes), Lua
embedded for lightweight in-app scripting, and the declarative shader format of §2.9 for
effects. WASM sandboxing for third-party plugin logic where we need untrusted code to be safe.

## Platforms

Windows and macOS at v1 (that is where the users and the money are), Linux close behind because
the render farm and the pipeline-integration users need it. iPad/Android is a Phase 4 question
that the architecture deliberately keeps open — the compute-only GPU path and the tiled
evaluator are the reason it stays possible.

## Notable non-choices

- **No game engine as a base.** Unreal/Unity bring an asset pipeline and editor model that
  fights everything in §2.
- **No web tech in the shell.** Discussed above; the cost lands squarely on the target user.
- **No third-party node/DAG framework.** The compiler is the differentiator.
