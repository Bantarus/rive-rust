# Milestone 2a — non-blocking GPU sync (drop the blocking fence)

**Status: complete.** M2a removes the per-frame **blocking `vkWaitForFences`** from the
zero-copy tier. rive now records its draws into **wgpu's own open command buffer**, so its
GPU work rides wgpu's single per-frame submit — GPU-ordered before the pass that samples the
shared image, with **no CPU stall, no separate submit, and no fence**. Output is **byte-identical**
to the proven M1b blocking path (opaque and `RIVE_CLEAR_ALPHA=0` transparent). The blocking path
is retained behind `RIVE_BLOCKING=1` as a fallback and an A/B baseline.

All numbers are measured on the **native RTX 4090** relay (`scripts/win.cmd`, driver 591.86,
`WGPU_BACKEND=vulkan`), **release** rive libs, `octopus_loop.riv` at **512×512** into the shared
`VkImage`, n=300 frames after a 30-frame warm-up. Every figure traces to a raw artifact:
[docs/perf/m2a_step0_raw.txt](perf/m2a_step0_raw.txt) (blocking) and
[docs/perf/m2a_step1_raw.txt](perf/m2a_step1_raw.txt) (non-blocking). Linux build stays green.

---

## Step 0 — measured target + regime (done before the rework)

A new shim CPU sub-span timer (`std::chrono::steady_clock`) brackets rive's `flush()` and the
blocking `vkWaitForFences` separately; the render-world collector now records, **per frame** (summed
over the frame's instances), four series: total submit wall, rive flush, fence wait, and rive GPU
command-buffer time. Knobs: `RIVE_PERF` / `RIVE_PERF_FRAMES`, and `RIVE_INSTANCES=N` (multi-instance).

### Step 0a — fence vs flush split (N=1, blocking)

| component | p50 (3 passes) | note |
|---|--:|---|
| frame CPU (total `render_external_frame` wall) | **607 / 663 / 694 µs** | rep. ~655 µs |
| rive `flush()` (CPU command record) | 95 / 92 / 91 µs | ~92 µs |
| **blocking `vkWaitForFences`** | **374 / 408 / 463 µs** | **~415 µs = ~63% of the wall** |
| rive GPU command buffer | 0.093 / 0.097 / 0.102 ms (p50); mean ~0.077 ms | ~77–97 µs |
| (remainder: begin/end CB, barrier, submit, ts readback) | ~150 µs | by difference |

**The frame is fence-bound, not flush-bound.** This refines the M2.0 assumption ("CPU-flush-bound"):
rive's actual CPU flush is only ~92 µs; the **blocking fence wait (~415 µs) is the single largest
component** and is ~4–5× the GPU execution time it waits on (~77–97 µs) — i.e. mostly
submit→GPU-start→fence-signal→CPU-wakeup latency, not GPU work. So the rework's recoverable target
is **large** (not the "small wait" the brief allowed for): roughly the fence wait plus the
out-of-band submit overhead.

### Step 0b — multi-instance baseline (frame-time vs N, blocking, p50)

| N | frame CPU | rive flush | fence wait | fence % of CPU | rive GPU (mean) | GPU % of CPU |
|--:|--:|--:|--:|--:|--:|--:|
| 1 | 655 µs | 92 µs | 415 µs | 63% | 0.077 ms | ~12% |
| 8 | 2258 µs | 364 µs | 1356 µs | 60% | 0.556 ms | ~25% |
| 32 | 4838 µs | 929 µs | 2707 µs | 56% | 1.563 ms | ~32% |
| 128 | 20071 µs | 5455 µs | 10369 µs | 52% | 5.553 ms | ~28% |

Reading the regime:
- **The fence dominates at every N (52–63% of frame CPU).** Dropping it is the highest-value change
  regardless of instance count.
- **Frame CPU grows ~linearly at high N** (32→128 is 4.0× count for 4.15× CPU); marginal per-instance
  cost converges to ~155 µs. Per-instance cost *falls* from N=1 (~655 µs) toward ~155 µs because the
  N=1 frame pays full per-frame GPU spin-up + submit/signal latency for only ~90 µs of GPU work,
  while back-to-back submits keep the GPU warm.
- **GPU is never the bottleneck in the blocking pipeline** (≤~32% of frame CPU even at N=128). At N≈80
  the rive fill alone exceeds a 60 fps (16.7 ms) budget.

**Verdict:** proceed with the sync rework — the blocking fence is the dominant, recoverable cost at
all N. (After it is gone, rive's CPU `flush()` becomes the next bottleneck — see Step 1 and M2 remainder.)

---

## Step 1 — non-blocking sync (the core change)

### Mechanism (verified against wgpu 27 / Bevy 0.18 / ash source)

Two designs were evaluated against the actual sources:

- **Inject a wait semaphore into Bevy's submit — NOT FEASIBLE in wgpu 27.** `wgpu_hal::vulkan::Queue::submit`
  ([wgpu-hal-27.0.4 vulkan/mod.rs:1458]) takes only a *signal* fence; its internal `wait_semaphores`
  is fed solely by surface-acquire + the relay chain, with no external input, and the only public
  injector (`add_signal_semaphore`) signals (wrong direction). Bevy's render graph does exactly one
  `queue.submit(commands)` ([bevy_render-0.18.1 graph_runner.rs:87]) with no hook to add a wait.
- **Unify rive into wgpu's command stream — FEASIBLE, and what M2a does.** `wgpu::CommandEncoder::as_hal_mut::<vulkan::Api>`
  → `CommandEncoder::raw_handle()` ([wgpu-hal-27.0.4 vulkan/mod.rs:1250]) exposes wgpu's *open* primary
  `VkCommandBuffer` for the frame (wgpu-core has already called `begin`). rive records its draws +
  its `COLOR_ATTACHMENT → SHADER_READ_ONLY` barrier into that buffer; the un-premult display pass
  then samples the shared texture **later in the same buffer / same submit**. Ordering and write→read
  visibility are intra-buffer (rive's barrier); no separate submit, no fence, no semaphore.

The new shim entry point `rive_frame_record_external(ctx, target, cmdBuffer)` records rive's flush +
barrier into the caller's open buffer and returns; `rive_frame_begin_external` / `rive_artboard_draw`
are unchanged. The wrapper adds `Context::record_external_frame`. The node fetches the raw buffer via
`as_hal_mut`, records, then records the display pass — all on the render thread (pipelining disabled).

### Frame pacing / resource recycling (and its bound)

With no fence, rive's transient pooled (host-mapped) buffers can no longer use `safe_frame = current − 1`
(that was safe *only because* the fence completed each frame). M2a uses **`safe_frame = current −
RIVE_RING_SIZE` (3)**, matching rive's `kBufferRingSize`: rive recycles a frame's buffer only after the
ring has wrapped. This is correct **while frames-in-flight ≤ 3**, which Bevy's default surface
guarantees (Fifo/AutoVsync, `desired_maximum_frame_latency` 2 → a 3-image swapchain caps CPU run-ahead
at ~3). Every M2a measurement ran under that default. Non-Fifo present modes or a higher frame latency
break the bound — see the review finding and guard below.

### Correctness validation (over many frames + varied load)

- **Frozen pose, blocking vs non-blocking, same build — md5-IDENTICAL** (opaque `1de62920…`; transparent
  `58c894e0…`; 0/255 per-channel delta via `tools/png_diff.py`). The rework is provably pixel-correct.
- **Realtime animation** (varied load): a coherent octopus at frame 200, no tearing/garbage.
- **Multi-instance** (N=8, realtime): all 8 instances render coherently at independent animation phases.
- **Many frames / clean runs:** 9 non-blocking perf runs (N=1/8/32/128, frozen + realtime), 300+ frames
  each — **zero panics / `VK_ERROR` / device-lost / validation errors**.
- **No regression:** M1a CPU-copy floor (`sprite_riv`, default features) runs on the 4090 and renders
  the octopus correctly; frozen M1a ECS API unchanged; both build green.

### Perf recovery vs the Step 0 target (p50; non-blocking artifact)

| N | blocking frame CPU | non-blocking frame CPU | fence (block→NB) | frame-CPU recovered |
|--:|--:|--:|--:|--:|
| 1 | 655 µs | **125 µs** (122/127/128, 3 passes) | 415 → **0** | **~81%** |
| 8 | 2258 µs | **384 µs** (378/390) | 1356 → 0 | ~83% |
| 32 | 4838 µs | 2200 – 3736 µs (2 passes) | 2707 → 0 | ~23–55% (variable) |
| 128 | 20071 µs | 8922 – 16022 µs (2 passes) | 10369 → 0 | ~20–56% (variable) |

- **N=1 is the clean headline:** frame CPU **655 → ~125 µs (~81%)**, fence wait → 0, output byte-identical.
  The Step 0 target (recover the ~415 µs fence wait) is met **and exceeded** (the ~150 µs out-of-band
  submit overhead is also gone). rive's `flush()` (~83 µs) is now the largest CPU component, as Step 0
  predicted. N=1/N=8 recoveries are stable across passes (≤3% spread).
- **The bottleneck FLIPS to the GPU at moderate N.** rive's GPU command-buffer time is *unchanged* by the
  rework (same recorded commands), but the CPU frame cost dropped below it: at **N=8 the GPU time
  (~0.56 ms mean / 0.65 ms p50) now exceeds the non-blocking CPU frame time (~0.38 ms)** → the pipeline
  is GPU-leaning there. This refines Step 0's "clockwise off the critical path": once the fence is gone,
  the ~23–28% GPU saving from clockwise (measured in M2.0) **does** start to matter around N≈8 — an M2
  decision once in-place upload lands. (So "GPU is never the bottleneck" holds for the *blocking* baseline
  only, not for the realized non-blocking pipeline.)
- **Honest caveat — rive's CPU `flush()` grows in the non-blocking path at high N, with high run-to-run
  variance.** At N=32, flush p50 goes ~929 µs (blocking) → 1732–2779 µs (non-blocking); at N=128, ~5455 µs
  → 6987–12389 µs (≈2–3×). Frame CPU at N≥32 swings widely between passes (N=128: 8.9–16.0 ms). With the
  fence gone, frame CPU ≈ flush, so the flush growth *is* the high-N cost. The likely cause is the CPU
  running far ahead of the GPU (variable resource pressure / cache locality, and N instances sharing one
  rive `Context` + frame number) — **not yet definitively measured**; bounding and reducing it (and
  per-instance contexts / frame numbering) is M2 remainder. **GPU timing is unavailable in the non-blocking
  path** (no completion signal to read timestamps without re-introducing a stall); Step 0's GPU figures
  apply to the rive command-buffer contents, which are identical.

---

## Step 2 — adversarial review (the deferred end-of-task review)

A 5-dimension find→verify review ran over the zero-copy + sync code (sync correctness, layout/tracker,
unsafe/FFI/shim, API-regression/collector, perf-honesty). **Layout/tracker and API-regression were clean**
(frozen API unchanged, collector correct, multi-instance example sound, `RIVE_BLOCKING` wired, ABI matches
exactly across `.h`/`.cpp`/`lib.rs`, no unsafe `Send/Sync` regression). Four findings were confirmed and
resolved:

| # | sev | finding | resolution |
|--:|---|---|---|
| 1 | medium | **`safe_frame = N−3` is silently violable** by non-Fifo present modes (Immediate/Mailbox/AutoNoVsync) or `desired_maximum_frame_latency ≥ 3` — the CPU can outrun GPU completion past the ring and rive overwrites a pooled buffer the GPU is still reading (silent corruption). Default config is safe. | Added a **one-shot runtime guard** in the node: reads the window's `present_mode` + `desired_maximum_frame_latency` from `ExtractedWindows` and **warns** (recommending `RIVE_BLOCKING`) when the in-flight bound could exceed `RIVE_RING_SIZE`. Strengthened the `RIVE_RING_SIZE` doc + wrapper safety contract. Verified silent under default Fifo. Robust fix (SubmissionIndex-derived watermark) flagged as **M2 remainder**. |
| 2 | low | **Stale module doc** claimed the wrapper handles are "`Arc`-refcounted (atomic), so cross-thread drop is sound" — they are non-atomic `Rc`; soundness comes from the `NonSend` single-thread invariant. | Corrected the comment to match the accurate wording used elsewhere; explicitly warns against re-enabling pipelining without switching to atomic refcounts. |
| 3 | medium | The **"rive flush grows at high N" disclosure was missing** from the perf narrative, and an artifact header could imply rive's per-frame cost was unchanged. | Added the disclosure (this report's Step 1 caveat) and re-scoped the artifact header to GPU command-buffer *contents* only. |
| 4 | medium | **"GPU never the bottleneck" overstated** for the non-blocking pipeline — at N=8 the GPU time exceeds the recovered CPU frame time. | Corrected in this report (the bottleneck-flip paragraph): the claim holds for the blocking baseline only. |

After the fixes: build green, clippy clean (0 warnings on our source), and the non-blocking frozen capture
re-confirmed **md5-identical** to the blocking path.

> Process note: the first review workflow run lost 4 of 5 dimensions (agents did the analysis but failed to
> emit structured output); it was re-run lean (find-only, explicit output mandate) and the findings above
> were then adversarially verified by reading the cited code. The perf re-measurement that backs this report
> was also re-run with direct-redirected logs after an earlier `tee` pipeline left empty artifacts — every
> number here traces to the saved `docs/perf/*` files.

---

## What changed (code)

- **`crates/rive-renderer-sys/shim/rive_shim.{h,cpp}`** — `rive_frame_record_external` (records rive's
  flush + `COLOR→SHADER_READ` barrier into the caller's open command buffer; no submit/fence); CPU sub-span
  timers (`extLastFlushUs`, `extLastFenceWaitUs`) + getters for the Step 0 split. Blocking
  `rive_frame_submit_external` unchanged.
- **`crates/rive-renderer-sys/src/lib.rs`** — FFI decls for `rive_frame_record_external` + the two `…_us` getters.
- **`crates/rive-renderer/src/lib.rs`** — `Context::record_external_frame` + `ExternalFrameRecord`;
  `last_flush_us` / `last_fence_wait_us`.
- **`crates/bevy-rive/src/zero_copy.rs`** — node uses `as_hal_mut().raw_handle()` → `record_external_frame`
  (default) with `safe_frame = current − RIVE_RING_SIZE`; `RIVE_BLOCKING` fallback; per-frame perf collector
  (flush/fence/CPU/GPU); the present-mode in-flight guard; corrected soundness doc.
- **`examples/sprite_riv_zerocopy.rs`** — `RIVE_INSTANCES=N` (grid-laid multi-instance), robust queries.
- **`tools/`, `docs/perf/`** — raw perf artifacts (`m2a_step0_raw.txt`, `m2a_step1_raw.txt`).

## Runtime knobs added (all default-off / behavior-preserving)

- `RIVE_BLOCKING=1` — use the M1b blocking submit+fence path (fallback / A-B baseline).
- `RIVE_INSTANCES=N` (default 1, clamp 1–1024) — spawn N independent rive instances (perf regime).
- (M2.0 knobs `RIVE_PERF` / `RIVE_PERF_FRAMES` / `RIVE_CLOCKWISE` / `RIVE_RUNTIME_*` unchanged.)

## M2 remainder (next, driven by these results)

1. **Robust recycling watermark** — derive `safe_frame` from wgpu's completed `SubmissionIndex`
   (`device.poll`) instead of a fixed `current − ring`, removing the present-mode precondition (finding #1).
2. **Reduce / bound rive's CPU `flush()` at high N** — it is now the bottleneck and is variable in the
   non-blocking path; investigate per-instance contexts / frame numbering and in-place upload (kill per-frame
   realloc). Characterize the run-to-run variance properly.
3. **Clockwise** — re-evaluate now that the pipeline is GPU-leaning at moderate N (it was off the critical
   path under blocking; it is ~23–28% less GPU at ≤1 LSB).
4. **Pipelining return** — only with a validated cross-thread *drop* strategy (atomic refcount or explicit
   main-thread teardown — never `Rc` + a ferried world), and only if the overlap is worth it.
5. **Native-Linux validation** when a bare-metal env exists.
