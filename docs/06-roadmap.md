# 6. Roadmap

Sequenced so that the riskiest assumption is tested first and every phase ships something a real
user can use. Timelines assume a **small senior team growing from ~6 to ~30**; halve or double
accordingly, but keep the *order*, because the order is the risk management.

---

## Phase 0 — Prove the thesis (months 0–4, ~6 people)

**No UI. No features. One question: is the engine actually faster?**

Deliverables:
- Benchmark harness and the reference-hardware CI fleet (§3.3) — this comes *first*
- Graph IR + compiler with pruning, constant folding, and **kernel fusion** (§2.3)
- RHI over Vulkan/Metal, compute-only path
- Tiled ROI evaluator with mip levels (§2.4)
- Content-addressed cache, all three tiers (§2.5)
- HW decode integration and the GOP index (§2.7)
- A headless renderer that can execute a graph to a file

**Exit criteria — all must pass, on the potato:**
1. A 15-node pointwise grade at 4K runs **≥4x** faster than the same graph with per-node buffers
2. 1080p H.264 + 6-node grade sustains real-time playback
3. ROI+mip evaluation on a windowed 4K viewport computes ≤15% of full-frame pixels
4. Peak VRAM under 1.5GB for the above

**If these fail, stop and redesign.** This phase exists specifically to be cheap to fail. Every
month spent on UI before this is answered is a month spent on a building without a foundation.

---

## Phase 1 — A real editor (months 4–14, → ~15 people)

Ship an NLE that a working editor can cut with. Narrow, but genuinely good.

- Custom UI toolkit (started in parallel at month 3 — it is on the critical path)
- Timeline: multi-track, full trim toolkit, keyboard-driven, Premiere/Resolve keymaps
- Media pool, import, automatic background proxies, relink
- Audio: mixer, clip effects, metering, waveform display
- Basic effects: transform, crop, opacity, transitions, essential filters
- Export with the queue and hardware encoders
- Project format, autosave, crash recovery
- OTIO/XML/EDL interchange, because nobody adopts a tool they can't get out of

**Milestone: alpha with ~50 working editors.** The bar is "I cut a paid job in it", not "it has
the features".

---

## Phase 2 — Colour (months 12–20)

- Full node grading UI, primaries, curves, qualifiers, windows
- Tracking for windows
- GPU scopes
- ACES/OCIO colour management surfaced properly
- LUTs, stills, versions, grade management
- HDR delivery

**Milestone: public 1.0.** Editing + colour is a coherent, sellable product. It is also the
point where the speed advantage becomes undeniable to anyone who tries it, because grading is
where fusion pays off most visibly.

---

## Phase 3 — Motion graphics and compositing (months 18–32)

- Keyframe system, graph editor, expressions
- Node editor and layer view as parallel idioms
- Text engine with animators; shape layers
- Masks, roto, planar and point tracking, camera solve
- Keying, despill, edge tools
- Particles
- Precomps, macros, and the effect authoring format opened up

**Milestone: 2.0.** This is where AE users can genuinely move, and where the "no preview wait"
advantage produces the demo that sells the product.

---

## Phase 4 — Breadth (months 28–48)

Running in parallel once the core three are solid:

- **Simple mode + templates + tutorials** (§5.6). Deliberately *late* in engineering order but
  early in marketing order — it is the growth engine, and it is also the thing most likely to
  compromise the pro product if built before the pro product is settled. Once the graph and
  macro system are stable, this is mostly content and UI, not engine work.
- **3D layer** (§2.12): USD import, cameras, lights, materials, hybrid renderer, AOV compositing
- **Render farm and CLI**, watch folders, smart passthrough
- **OFX host** for third-party plugin compatibility
- **Collaboration** on the command log; shared cache; review/web viewer
- **Mobile/tablet viability probe**

---

## Phase 5 — Ecosystem (year 4+)

Effect marketplace, template economy, deeper Blender interop, pipeline integrations, whatever
the users have by then made obvious.

---

## Sequencing rationale

Three decisions in this order are worth defending explicitly:

**Why engine before UI.** The performance thesis is the entire reason this product would exist.
It is also the only part that cannot be fixed later. Test it while it is cheap to be wrong.

**Why colour before motion graphics.** Colour is a smaller surface area, it is where fusion
produces the most spectacular and most *demonstrable* win, and Resolve's free tier means colour
users are already comfortable adopting a second tool. It gets us a shippable 1.0 sooner.

**Why Simple mode last-ish.** Every product that builds the beginner mode first ends up with the
beginner mode as the architecture, and the pro features become bolt-ons that fight it. Build the
capable thing, then present a simple view of it. The reverse does not work — and CapCut's own
ceiling is the evidence.
