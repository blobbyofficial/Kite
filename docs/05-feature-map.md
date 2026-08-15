# 5. Feature Map — How Each Reference App Lands in One Product

For each source application: what we take, how it maps onto the architecture in
[02-architecture.md](02-architecture.md), and what the honest "good enough to switch" bar is.
The bar matters more than the feature list — parity is a trap, *switchability* is the goal.

---

## 5.1 Premiere Pro → the timeline for long-form

**What we take:** a professional, keyboard-driven timeline that stays fast at feature length.

**Mapping:** the timeline is a temporal view compiling to the graph (§2.2). The immutable
document model (§2.10) is what keeps a 10,000-clip sequence responsive — edits are structural
diffs against a shared tree, so cost scales with the size of the *edit*, not the sequence.

**Must have:**
- Three/four-point editing, ripple/roll/slip/slide, J/L cuts, trim mode
- Multicam with synchronised angles and live switching
- Nested sequences (which are literally subgraphs — free from the architecture)
- Proper media management: relink, consolidate, offline handling, EDL/AAF/XML/OTIO
- Speed ramping with optical-flow retiming
- A real audio mixer: buses, sends, keyframed automation, loudness metering to broadcast specs
- Customisable keymaps, **including Premiere and Resolve presets** — muscle memory is the single
  largest switching cost, and shipping a faithful keymap is cheap

**Bar to switch:** an editor cuts a 45-minute piece end to end without hitting a wall, and the
timeline is as responsive in hour six as in minute one.

---

## 5.2 DaVinci Resolve → colour

**What we take:** node-based grading with real scopes and real colour science.

**Mapping:** grading nodes are the *ideal* fusion candidates (§2.3) — almost all pointwise, so a
20-node grade compiles to one dispatch. This is where our speed advantage is most visible and
most dramatic. ACES/OCIO pipeline per §2.11.

**Must have:**
- Primaries (lift/gamma/gain, offset, log wheels), curves (all channel and HSL variants)
- Qualifiers (HSL keying) and power windows with tracking
- Node graph: serial, parallel, layer, key mixing, outside nodes
- Scopes: waveform, vectorscope, parade, histogram — GPU-computed at display resolution, never
  blocking the frame
- LUT import/export, colour-managed throughout
- Grade copy/paste, stills gallery, versions per clip
- Tracking that drives windows, and a decent auto-balance for the impatient

**Bar to switch:** a colourist grades a short film without reaching for Resolve, and grade
adjustments feel *immediate* — no render cache, no wait, on a laptop.

---

## 5.3 After Effects + Fusion → motion graphics and compositing

**What we take:** a genuinely capable compositor, in both node and layer idioms.

**Mapping:** this is what the graph was designed for. Both the node view and the layer view are
views on the same DAG (§2.2), so an artist can flip idiom mid-project — something neither AE nor
Fusion offers.

**Must have:**
- Full keyframe system: bezier interpolation, graph editor, easing, motion blur
- Expressions/parametric links between any parameters (JS-compatible syntax for AE refugees)
- Text engine with animators, per-character control, and real typography
- Shape layers, masks, roto with tracking assistance
- Planar and point tracking; camera solve
- Keying (chroma/luma/difference), despill, edge treatment
- Particles, and a 2.5D/3D camera space consistent with §2.12
- Precomps — again, subgraphs, free from the architecture

**The differentiator:** AE's defining friction is the RAM-preview cycle — change something, wait,
watch. With ROI-driven tiled evaluation (§2.4) and a persistent cache (§2.5), the common case is
that you simply *see* the change. Removing the wait cycle changes how the work feels more than
any individual feature.

**Bar to switch:** a motion designer builds a 30-second title sequence with tracking, type, and
particles, and never once waits for a preview render.

---

## 5.4 Blender → 3D

**Scope-limited by design.** See §2.12. We build the layer, the interchange, the hybrid
renderer, and native AOV/deep compositing. We do not build modelling, sculpting, or simulation.

**Bar to switch:** a motion designer imports a USD scene, animates the camera, lights it, renders
with AOVs, and composites — without leaving the app or writing intermediate EXR sequences to
disk. That is the workflow people actually need; the modelling happens upstream regardless.

---

## 5.5 Media Encoder / HandBrake → delivery

**What we take:** a serious, independent delivery pipeline.

**Mapping:** a separate render engine process sharing the same content-addressed cache (§2.5) —
so anything already reviewed at full quality is a cache hit rather than a re-render.

**Must have:**
- A render queue that runs while you keep editing, without degrading the edit
- Hardware and software encoders across a quality/speed ladder (H.264, HEVC, AV1, ProRes,
  DNxHR, image sequences)
- Preset system with the platform presets people actually need, plus custom
- Watch folders and CLI/headless operation (`app-cli render project.vep --preset youtube-4k`)
- **Smart passthrough** (§2.7) — untouched segments in the delivery codec are copied at the
  bitstream level
- **LAN render farm**: any installed copy can be a node; jobs distribute by frame range with a
  shared cache. Two idle office machines should halve an export.
- Per-shot re-render: change one grade, re-encode only the affected GOPs

**Bar to switch:** someone uninstalls Media Encoder, and a long export finishes in a fraction of
the time because most of it never needed re-encoding.

---

## 5.6 CapCut → approachability

This is the one most easily done badly. A "beginner mode" bolted onto a pro app is usually a
worse CapCut *and* a compromised pro app. The way through is that Simple mode is not a reduced
feature set — it is **a different view over the same document**, with the same engine.

**Three interface levels, one document:**

| Level | Shows | Hides |
|---|---|---|
| **Simple** | Single-track-feeling timeline, template gallery, one-click looks, auto-captions, auto-cut | Node graph, scopes, colour management, precise trim modes |
| **Standard** | Full multi-track timeline, layer-based effects, basic grading | Node editor, deep compositing, colour science |
| **Pro** | Everything | Nothing |

Critically: a project made in Simple opens in Pro as a **real, fully editable graph** — no
conversion, no lossy migration, nothing baked. That upgrade path is the product's long-term
retention story. The teenager who starts on Simple mode has no reason to ever leave.

**Must have:**
- A large library of templates that are **graph macros** (§2.9) — meaning a shared template runs
  at the same speed as a built-in effect, and a Pro user can open one up and see how it works
- Auto-captions (on-device speech recognition — no cloud dependency, works offline)
- Auto-reframe for aspect ratios, beat detection, silence removal, background removal
- One-click looks and transitions that are, underneath, ordinary node chains
- Direct export presets for the platforms people actually publish to

**Built-in interactive tutorials.** Not videos — the tutorial system drives the app through the
same command log as the user (§2.10), so a lesson can highlight real UI, perform a real edit,
hand control back, and *verify* the user's result by inspecting the document. Tutorials ship as
data, are authored by recording a session, and can be community-made.

**Bar to switch:** a 14-year-old with no editing experience makes a captioned, graded, published
video in 15 minutes on a Chromebook-class laptop, and never sees a word of jargon.

---

## 5.7 What each source contributes, at a glance

| Source | Core contribution | Architectural home |
|---|---|---|
| Premiere | Long-form timeline, media management | Timeline view + document model |
| Resolve | Colour, scopes, node grading | Fusion-friendly pointwise node chains |
| After Effects | Keyframes, expressions, text, layer idiom | Layer view over the DAG |
| Fusion | Node compositing, tracking, keying | Node view over the DAG |
| Blender | 3D scenes, lighting, rendering | 3D layer + USD + AOV compositing |
| Media Encoder | Queue, presets, farm, delivery | Render process + shared cache |
| HandBrake | Encoder control, batch transcode | Same, with a simpler front end |
| CapCut | Templates, automation, teaching | Simple view + graph macros |
