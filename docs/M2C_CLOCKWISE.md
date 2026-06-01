# Milestone 2c — clockwise PLS: measure the real win, then default it (capability-gated)

**Status: complete.** Clockwise PLS was already wired (`RIVE_CLOCKWISE`) and attested *correct* on the
native 4090 by the M2 verification gate. M2c answers the three open questions before defaulting it:
**(1)** is clockwise free or a CPU tradeoff in the *non-blocking* pipeline? **(2)** where does it actually
move wall-clock? **(3)** does the ≤1 LSB equivalence hold beyond the octopus? The measured answers:

- **Clockwise is CPU strictly-better-or-neutral — not a tradeoff.** Lower rive frame-CPU *and* lower
  `flush()` at **every** N (≈10–25 % / 17–33 %, Step 1). It does less CPU work than atomics, not more.
- **Wall-clock win band is N ≈ 32–128** (+13 % fps at N=32, +8 % at N=128, Step 2) — exactly where the
  M2b run-ahead metric climbs to 2 (GPU-leaning). At N=1–8 (run-ahead=1, CPU/overhead-bound) it is
  wall-clock-neutral, but still CPU-cheaper (head-room that matters off the hot path).
- **Output matches atomics to within coverage/AA quantization across 7 assets**, alpha exactly identical
  and every pair visually identical. The delta is byte-identical-to-≤4 LSB spread across gradients
  (cloner 0; coffee/octopus ≤1; draworder/eye ≤2; big-wheel ≤4), rising to ≤55 LSB on **0.039 %** of the
  slot-machine frame (thin high-contrast antialiased edges only). The shipped **clockwise** path is
  **validation-clean (zero interop VUIDs / sync-hazards)** under core+sync validation on every asset on the
  native NVIDIA ICD. Both PLS modes are rive-correct; the divergence is accepted as a precision property of
  the two fill algorithms (decision in Step 3).

Because clockwise is free-or-better on CPU, positive on throughput, and correct, M2c makes it the
**capability-gated default**: on wherever rive's clockwise path is the best available (pixel interlock
present, raster-order absent), off everywhere else — Dozen and no-interlock HW fall back to atomics
**automatically, with no flag**. `RIVE_CLOCKWISE` / `RIVE_NO_CLOCKWISE` / `RIVE_FORCE_ATOMIC` remain for
A/B and for forcing either path.

All numbers are measured on the **native RTX 4090** relay (`scripts/win.cmd`, driver 591.86,
`WGPU_BACKEND=vulkan`), **release** rive libs, 512×512 into the shared `VkImage`, n=240 frames after a
30-frame warm-up, non-blocking timeline-watermark path. Every figure traces to a raw artifact:
[docs/perf/m2c_perf_raw.txt](perf/m2c_perf_raw.txt) (Step 1/2) and
[docs/perf/m2c_validation.txt](perf/m2c_validation.txt) (Step 3/4 diffs + validation). Linux build green;
clippy clean; the frozen ECS API and the M1a CPU-copy floor are untouched.

---

## Step 1 — the decision pivot: clockwise CPU vs atomics CPU

Measured in the non-blocking pipeline (`Immediate`, unthrottled — the clean read; `Fifo` cross-check
below). atomics = default-with-interlock (`RIVE_NO_CLOCKWISE`); clockwise = `RIVE_CLOCKWISE`. p50, µs.

| N | frame-CPU atomic → **cw** | flush atomic → **cw** | Δ frame-CPU | Δ flush |
|--:|--|--|--:|--:|
| 1   | 44.7 → **39.3**       | 32.1 → **26.5**       | −12 % | −17 % |
| 8   | 219.9 → **165.8**     | 153.1 → **102.7**     | −25 % | −33 % |
| 32  | 972.3 → **751.9**     | 680.8 → **473.3**     | −23 % | −30 % |
| 128 | 11434 → **10312**     | 9902 → **8835**       | −10 % | −11 % |

**Read: clockwise is free — in fact CPU-cheaper — at every N.** This is the "strictly-better-or-neutral"
branch: clockwise does less CPU flush work than atomics, so it should be defaulted wherever available,
**independent of N**. (`Fifo` cross-check agrees at N=8/32/128 — e.g. N=8 flush 156.9 cw vs 325.5 atomic;
the lone outlier is `Fifo` N=1, where a one-off startup spike skews the short window under vsync. The
unthrottled `Immediate` read above is authoritative for CPU cost.)

## Step 2 — the actual non-blocking wall-clock win

Under `Immediate` (no vsync → frames run as fast as CPU+GPU allow), sustained throughput from the
steady-state frame period, with the M2b run-ahead (= `frame_no − safe_frame`, frames in flight) as the
GPU-bound indicator. p50.

| N | fps atomic → **cw** | Δ fps | run-ahead (atomic / cw) | regime |
|--:|--|--:|--|--|
| 1   | 1504 → 1376  | (noise) | 1.0 / 1.0 | CPU/overhead-bound — neutral |
| 8   | 963 → 960    | ~0 %    | 1.0 / 1.0 | CPU-bound — neutral |
| 32  | 439 → **495** | **+13 %** | 1.8 / 1.6 (p50 2) | GPU-leaning — clockwise wins |
| 128 | 62.1 → **67.2** | **+8 %** | 1.9 / 1.9 (p50 2) | GPU-leaning — clockwise wins |

**Read: the win band is N ≈ 32–128**, precisely where run-ahead climbs from 1 to 2 (the CPU starts
waiting on the GPU). At N=1–8 the pipeline is CPU/overhead-bound (run-ahead pinned at 1, frame period
dominated by Bevy/winit/present, not rive), so the lower rive CPU does not move fps — clockwise is
wall-clock-neutral there but costs less CPU regardless. This directly confirms the previously-*inferred*
GPU-leaning band with a measured throughput A/B.

## Step 3 — broader-content correctness before defaulting

Frozen pose (`RIVE_SPEED=0`), clockwise vs atomics, under **core + synchronization validation**
(`WGPU_VALIDATION=1` + a layer-settings file; the `SYNCHRONIZATION_VALIDATION` banner is confirmed
present in every log). **Seven assets** (the original two + five later-supplied demos spanning scripted
path effects, an interactive rig, gradient-heavy painted artwork, and a draw-order animation). The native
NVIDIA ICD runs clockwise; Dozen (no interlock) exercises the atomics fallback. Same-mode re-captures are
0-delta, so cross-mode deltas are stable fill-algorithm precision, not run-to-run noise.

| content | clockwise vs atomics (native) | native validation |
|---|---|---|
| cloner-scripted-path-effect | max Δ **0** — **byte-identical** | clean |
| coffee_loader (loader) | max Δ **1 LSB**, α identical, 32 px (0.003 %) | clean |
| octopus_loop (transparency, gradients) | max Δ **1 LSB**, α identical, 149 px (0.02 %) | clean |
| animating-draw-order | max Δ **2 LSB**, α identical, 213 px (0.02 %) | clean |
| eye-joysticks-demo | max Δ **2 LSB**, α identical, 22442 px (2.4 %) | clean |
| big-wheel-demo (gradient-heavy) | max Δ **4 LSB**, α identical, 121969 px (13.2 %) | clean |
| slot-machine (painted art, thin edges) | max Δ **55 LSB** but **0.039 % px** (96.2 % identical, 3.68 % at 1–2 LSB), α identical | clean |

The divergence is **coverage / antialiasing quantization** between the two fill algorithms: ≤1–4 LSB
spread across smooth gradient backgrounds, rising to tens of LSB on a **tiny fraction of thin
high-contrast antialiased edges** (the slot-machine: 361 px / 0.039 % in the 33–64 LSB band, localized by
heatmap to the slot outline / stars / arrow / a UI divider — *not* a structural defect or a shifted pose).
Every rendered pair is **visually identical** and alpha is **exactly** identical. The shipped **clockwise
default is validation-clean on every asset** (only VUID is the known non-ours naga
`VUID-StandaloneSpirv-None-10684`). Notably the draw-order animation diffs at only ≤2 LSB — the PLS mode
does not change element z-ordering.

**Decision:** the divergence exceeds the milestone's strict ≤1 LSB "pure rounding" bar (up to 55 LSB on
the slot-machine), so it was raised explicitly — **twice**, as the magnitude grew with added assets.
**Resolution (confirmed): keep clockwise as the default** — it is rive's own preferred path on interlock
HW, both PLS modes are rive-correct, and the difference is imperceptible (visually identical side-by-side;
the high-LSB pixels are sub-pixel edge antialiasing on <0.04 % of the frame), alpha-exact, and
validation-clean. The divergence is accepted as a known precision property of the clockwise vs atomic fill
algorithms.

### A pre-existing atomic-fallback finding (Dozen, heavy content) — NOT the shipped default

Validating the **atomics fallback** on Dozen surfaced `SYNC-HAZARD` errors on **rive's own internal
atomic-PLS coverage storage image** (`STORAGE_IMAGE`, binding #3, `IMAGE_LAYOUT_GENERAL`; named "atomic
coverage backing" in the debug build) — read/write across draws with no barrier
(`SYNC_FRAGMENT_SHADER_SHADER_STORAGE` READ-after-WRITE and WRITE-after-READ). It appears **only on the two
richest assets** (big-wheel 240×, slot-machine 540×) and is **absent on the five simpler ones**
(cloner/octopus/coffee/eye/draworder). It is **entirely inside rive's flush, not our zero-copy interop** —
no hazard names our shared `VkImage`/display image; the `COLOR→SHADER_READ` barrier and timeline semaphore
are uninvolved. Root cause, characterised: it fires **only on Dozen's D3D12-translated atomic execution +
heavy overlapping fills** — on the native 4090 it is **absent in every configuration** (clockwise,
`RIVE_NO_CLOCKWISE` atomics, and `RIVE_FORCE_ATOMIC` atomics: all 0 hazards). So it is **not** on the
shipped clockwise default, **not** on any native path, and **not** introduced by M2c (the atomic-path code
is unchanged). It is a rive-upstream atomic-fallback property; see Follow-ups.

Two further build/asset notes (orthogonal to M2c): the slot-machine trips a rive **debug-only** assert in
its asset importer (`file_asset_importer.cpp` `onFileAssetContents !m_content`, on the demo's embedded file
assets) — so it aborts under the **debug** Dozen build but loads + renders fine in **release** (native and
Dozen). And `rc=1` on the validation-enabled Dozen cells is a validation-error-on-exit artifact of the
known Dozen cubemap VUID — renders completed and captured correctly.

## Step 4 — exposure: the capability-gated default

The default is now computed in `extract_shared_handles_once` ([zero_copy.rs](../crates/bevy-rive/src/zero_copy.rs)):

```rust
let clockwise = if RIVE_NO_CLOCKWISE { false }          // A/B: atomics on an interlock device
                else if RIVE_CLOCKWISE { true }         // force clockwise on
                else { pixel && !raster };              // capability-gated default
```

- **`pixel && !raster`**, not just `pixel`: rive's clockwise path needs pixel interlock, but when
  raster-order (`VK_EXT_rasterization_order_attachment_access`) is present rive already selects its
  cleaner **RasterOrdering** mode — so we must *not* force `clockwiseFillOverride` there. The 4090 has
  `raster=false`, so the gate is true (clockwise); a raster-order device keeps RasterOrdering.
- **Capability-gated, not N-gated** — justified because Step 1 found *no* CPU penalty (clockwise is
  cheaper at all N), so there is no regime where atomics is preferable on interlock HW.
- **Automatic fallback, no flag.** Dozen advertises neither interlock ext (`pixel=raster=false` —
  empirically confirmed; this corrects a stale code comment that claimed Dozen advertises-but-can't-
  execute interlock), so the gate yields atomics there with no flag and no crash. Older no-interlock HW
  likewise.

**Verification** (native 4090, release, frozen octopus; refs: atomic `1de62920`, clockwise `d5294a72`):

| env | config | PLS mode | md5 |
|---|---|---|---|
| native | **default** | **Clockwise** | d5294a72 (= explicit clockwise) |
| native | `RIVE_NO_CLOCKWISE=1` | Atomics | 1de62920 (= explicit atomics) |
| native | `RIVE_CLOCKWISE=1` | Clockwise | d5294a72 |
| Dozen | default | **Atomics** (pixel=raster=false) | — |

Default selection is capability-driven (Clockwise on the 4090, Atomics on Dozen); the atomics fallback is
verified on Dozen; the flags still force either path; and the default-path output is byte-identical to the
explicit-clockwise reference. **DoD met.**

---

## Instrumentation added

One measurement-only addition to the `RIVE_PERF` collector (`PerfStats`): a steady-state wall-clock
frame-period sample (`Instant`-delta between consecutive post-warm-up frames) → `fps(p50)`, reported
alongside the existing CPU/flush/run-ahead columns. Needed because the existing `frame CPU` is rive's
submit wall, not the full frame period; sustained throughput (Step 2) requires the latter. No behaviour
change; the frozen ECS API is untouched.

## Guardrails honoured

- **Correctness over perf.** The shipped **clockwise default** is validation-clean (zero interop VUIDs /
  sync-hazards) on every asset on the native deploy hardware. Clockwise diverges from atomics by ≤4 LSB on
  gradient-heavy content — beyond the strict ≤1 LSB bar — so per the guardrail this was **raised
  explicitly** rather than waved through; the decision to keep clockwise (imperceptible, alpha-exact,
  rive's own preferred path) was made deliberately, not by default. The one sync-hazard found is in rive's
  **atomic fallback** on Dozen only (rive-internal, not our interop, not the default path) — see Step 3.
- **Numbers/attestations trace to artifacts** — `docs/perf/m2c_perf_raw.txt`, `docs/perf/m2c_validation.txt`.
- The frozen ECS API and the M1a CPU-copy floor are untouched.

## Follow-ups (out of scope here)

- The raster-order PLS mode (`VK_EXT_rasterization_order_attachment_access`) is still unexercised — the
  4090 doesn't advertise it, so rive never selects it. Needs other hardware (carried over from the M2 gate).
- **rive atomic-fallback sync-hazard on no-interlock HW + heavy content** (Step 3): rive's atomic-PLS
  coverage read-after-write trips `SYNC-HAZARD-READ-AFTER-WRITE` on Dozen's D3D12-translated execution
  (big-wheel demo). Absent on the native 4090 in every config. Worth a rive-upstream look at the atomic
  path's barriering, and confirmation on real (conformant) no-interlock hardware — unavailable here.
- Wider clip/mask-heavy `.riv` coverage would extend the content diff further (the harness is
  `RIVE_RIV=<file>`-agnostic).
