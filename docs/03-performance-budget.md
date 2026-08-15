# 3. Performance Budget

Performance goals that are not measured are decoration. Every number here is a **build-breaking
CI gate**, checked on real reference hardware, from the first month of Phase 0.

## 3.1 Reference machines

| Tier | Spec | Role |
|---|---|---|
| **Potato** | 2018 ultrabook, 4c/8t, Intel UHD 620, 8GB RAM, SATA SSD | The target. Gates block on this. |
| **Mainstream** | M1 MacBook Air 8GB / Ryzen 5 + GTX 1650, 16GB | Expected median user |
| **Workstation** | 16c, RTX 4070+, 64GB, NVMe | Headroom validation, farm node |
| **Mobile** | Recent Android / iPad | Phase 4 viability probe only |

## 3.2 Budgets

### Startup and responsiveness
| Metric | Potato | Mainstream |
|---|---|---|
| Cold start → editable timeline | < 2.0s | < 1.0s |
| Open 90-min project (warm cache) | < 3.0s | < 1.5s |
| Any UI frame during interaction | < 16ms (p99) | < 8ms (p99) |
| Trim/ripple response, 10k-clip timeline | < 16ms | < 16ms |
| Keystroke → character on screen | < 30ms | < 20ms |

### Playback (real time, no dropped frames)
| Content | Potato | Mainstream |
|---|---|---|
| 1080p30 H.264, cuts only | ✅ | ✅ |
| 1080p30 + 6-node grade | ✅ | ✅ |
| 4K30 HEVC, cuts only | ✅ (via proxy) | ✅ (native) |
| 4K30 HEVC + 6-node grade | ✅ (via proxy) | ✅ |
| 1080p + 20-node comp w/ blur & keyer | ≥ 15fps preview | ✅ |

### Memory and footprint
| Metric | Ceiling |
|---|---|
| Idle RSS | 400MB |
| 90-min timeline, active editing | 2GB |
| VRAM on a 2GB GPU | Must not exceed; degrade instead |
| Installer size | 500MB |
| Disk cache | User-capped, default 10% of free space |

### Export
| Metric | Target |
|---|---|
| 10-min 1080p, graded, HW encode, mainstream | < 2 min |
| Smart-passthrough on unmodified segments | ≥ 20x realtime |
| Export while editing | Editing stays interactive; export yields |

## 3.3 How these are enforced

- **Benchmark harness before feature code.** Phase 0's first deliverable is the measurement
  rig, not the renderer.
- **A CI fleet of real reference machines**, not cloud VMs — iGPU behaviour and thermal
  throttling do not reproduce on a virtualised runner, and those are exactly what we are
  optimising for.
- **A corpus of ~30 real project files** (a wedding, a 90-min doc, a heavy motion-graphics comp,
  a 4K multicam, a 10,000-clip stress case) replayed on every merge.
- **Frame-time histograms, not averages.** p99 is the number a user feels. A 60fps average with
  a 200ms hitch every two seconds is a bad editor.
- **A regression is a build failure.** Not a ticket, not a follow-up. The moment "we'll optimise
  it later" becomes acceptable, this project becomes another slow NLE with a good README.

## 3.4 Degradation strategy

Never stutter, never block, never freeze. When the machine cannot keep up, the engine degrades
along an explicit ladder, in this order, and always recovers on idle:

1. Drop to a lower mip level (visually mildest, largest win)
2. Skip `quality:refine`-marked nodes (fine denoise, high-quality resampling, extra AA)
3. Serve the last-good cached frame for tiles still in flight
4. Drop playback to a lower frame rate — **but keep audio continuous and in sync**, because
   users tolerate visual degradation far better than audio glitches
5. Surface a small, honest indicator of what is being degraded — never a modal, never a beach
   ball

The inverse also matters: on idle, refine quietly to full quality without the user asking.
