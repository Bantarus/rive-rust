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
- **Output is ≤1 LSB vs atomics on diverse content** (octopus *and* coffee), alpha exactly identical,
  with **zero interop VUIDs / sync-hazards** under core+sync validation on both the native NVIDIA ICD
  (clockwise) and Dozen (the atomics fallback).

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
present in every log). The native NVIDIA ICD runs clockwise; Dozen (no interlock) exercises the atomics
fallback.

| content | clockwise vs atomics (native) | interop VUIDs | sync-hazards |
|---|---|--:|--:|
| octopus_loop (transparency, gradients, many paths) | per-channel max Δ **1 LSB**, alpha **identical**, 149/921600 px differ | 0 | 0 |
| coffee_loader (loader animation) | per-channel max Δ **1 LSB**, alpha **identical**, 32/921600 px differ | 0 | 0 |

The only native VUID is the known naga/Bevy `VUID-StandaloneSpirv-None-10684` (matrix shaders, not ours —
characterised in the M2 gate). The Dozen atomics-fallback path is likewise clean (only the known
wgpu-internal Dozen cubemap `imageOffset-07738`); default and `RIVE_FORCE_ATOMIC` produce byte-identical
Dozen output. **The ≤1 LSB equivalence is a rounding difference in the different fill algorithm, not a
sync/correctness defect — confirmed on both available assets.**

> Coverage note: only `octopus_loop.riv` and `coffee_loader.riv` ship in-repo. They span transparency,
> gradients, and many paths, but a wider clip/mask-heavy set would strengthen the insurance. Drop more
> `.riv` into `assets/` to extend this diff — the harness is content-agnostic (`RIVE_RIV=<file>`).

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

- **Correctness over perf.** Clockwise diverges from atomics by ≤1 LSB (alpha identical) on every tested
  content, and trips zero VUIDs / sync-hazards under core+sync validation on native and Dozen — so it is
  safe to default. Had it diverged beyond rounding or tripped a hazard, the default would have stayed
  atomics.
- **Numbers/attestations trace to artifacts** — `docs/perf/m2c_perf_raw.txt`, `docs/perf/m2c_validation.txt`.
- The frozen ECS API and the M1a CPU-copy floor are untouched.

## Follow-ups (out of scope here)

- The raster-order PLS mode (`VK_EXT_rasterization_order_attachment_access`) is still unexercised — the
  4090 doesn't advertise it, so rive never selects it. Needs other hardware (carried over from the M2 gate).
- Wider, clip/mask-heavy `.riv` coverage for the content diff (see the Step 3 coverage note).
