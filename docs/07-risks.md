# 7. Risks and Open Questions

Ordered by expected damage. A plan without this section is a pitch.

## R1 — The fusion thesis underdelivers *(critical)*

The entire premise is that kernel fusion plus ROI/mip evaluation yields a step-change on
low-end hardware. If the real win is 1.5x rather than 5x, we are building a worse Resolve.

*Plausible failure modes:* register pressure in large fused kernels causing occupancy collapse;
old integrated drivers miscompiling generated shaders; shader compilation stalls on first use.

*Mitigation:* Phase 0 exists solely to answer this, before any UI investment. Fusion groups are
size-capped and tuned per driver class. Generated kernels are disk-cached and pre-warmed. If
the answer is bad, we learn it in month four for the price of six engineers for four months.

## R2 — Scope collapse *(critical, and the most likely way this actually dies)*

Six flagship apps is not a scope, it is a wish. The failure pattern is predictable: a thin
imitation of everything and a good version of nothing, shipped years late.

*Mitigation:* the phase gates in [06-roadmap.md](06-roadmap.md) are real gates. Each phase must
be independently shippable and independently good. "Good enough to switch" bars per feature area
(§5) instead of parity checklists. 3D scope-limited from day one and defended. Every "while
we're in there" is a schedule risk with a name.

## R3 — Low-end GPU driver reality *(high)*

Old integrated GPU drivers are a minefield: miscompiles, missing extensions, silent wrong
results, vendor-specific crashes. Our primary target is precisely the hardware with the worst
drivers.

*Mitigation:* compute-only path to minimise surface area; a driver quirk database with
per-vendor fallbacks; conformance tests in CI on real hardware; a genuinely usable SIMD CPU
backend as the floor. Budget real, ongoing engineering for this — it is not a one-time cost.

## R4 — Codec licensing *(high, and often underestimated)*

H.264 and HEVC carry patent licensing obligations with per-unit costs that can be material at
consumer scale. HEVC in particular has multiple pools. This is a business-model constraint that
must be settled before pricing, not after.

*Mitigation:* legal review early. Lean on OS-provided codecs where the licence travels with the
platform. Push AV1/VP9 where we can. Model the per-unit cost into pricing from the start. A
free tier may need a codec-restricted delivery set.

## R5 — The custom UI toolkit consumes the project *(high)*

Building a GPU-drawn, accessible, cross-platform UI toolkit is a multi-year project on its own.
It is also non-negotiable (§4) — and it is where this kind of project most often quietly
capitulates to Electron and permanently loses the performance story.

*Mitigation:* scope it to *this application's* needs, not a general-purpose framework. Dedicated
team from month three. Accessibility from the start — retrofitting it is far more expensive.
Re-evaluate honestly at month nine; if it is failing, the fallback is a native-widget shell with
a GPU viewport, which costs polish but not the thesis.

## R6 — Adoption inertia *(medium-high)*

Professional editors switch tools reluctantly and at real cost. Being faster is not automatically
enough; Resolve's free tier already sets a brutal price floor.

*Mitigation:* interchange in and out (OTIO/AAF/XML/EDL) so trying us is low-risk; keymap
compatibility so muscle memory survives; target the underserved low-end user first, where the
pain is greatest and the incumbents are weakest; a generous free tier where our low cost of
goods is an advantage.

## R7 — The plugin ecosystem chicken-and-egg *(medium)*

Pros depend on plugins they already own. No users means no plugin developers.

*Mitigation:* OFX host (§2.9) for compatibility from Phase 4; make first-party effects excellent
so third-party is a bonus not a requirement; make the declarative effect format so easy to
author that the effect library grows from the community; graph macros mean users create shareable
"effects" without writing any code at all.

## R8 — Colour science credibility *(medium)*

Colourists will not trust a new tool's maths. Reputation here is slow to earn and instant to
lose.

*Mitigation:* ACES/OCIO rather than in-house science; publish test results against reference
transforms; hire a known colour scientist; ship the conformance suite publicly.

---

## Open questions

1. **Business model** — perpetual, subscription, or Resolve-style free-with-paid-studio? This
   interacts directly with R4 and R6 and should be settled before Phase 1 ends.
2. **How free is the free tier?** Resolve set the expectation. Our low-end focus argues for
   generosity, since those users are also the least able to pay.
3. **Web viewer scope** — review-and-comment only, or actual editing? Affects how much of the
   engine needs a `wgpu` path.
4. **Mobile** — the architecture keeps it possible. Is it a product? Different question, and it
   should not be answered by drift.
5. **Open source?** Opening the engine could accelerate the driver-quirk work (R3) and adoption
   (R6) considerably. Opening the effect format and project format costs little and buys trust.
6. **Team shape** — the plan needs GPU compiler engineers, codec engineers, colour scientists,
   and UI toolkit engineers. That is four scarce specialisms simultaneously, and hiring is a
   schedule risk not yet priced in this document.
