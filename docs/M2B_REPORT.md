# Milestone 2b — robust recycling watermark + high-N flush characterization

**Status: complete.** M2b replaces the M2a fixed `safe_frame = current_frame − RIVE_RING_SIZE(3)`
recycle watermark — sound only while frames-in-flight ≤ 3 (Bevy's default Fifo) — with an **exact,
non-blocking GPU-completion watermark** read from **our own Vulkan timeline semaphore**. wgpu signals
that semaphore with the frame number on each of its per-frame submits, so reading its counter tells
rive precisely which frames' transient buffers the GPU has finished with. This **removes the
present-mode precondition** the M2a review flagged (Immediate / Mailbox / high frame latency are now
safe), and — measured below — also **roughly halves rive's high-N CPU flush**, because the old fixed
offset over-stated run-ahead (3 frames vs the real ~1) and inflated rive's transient working set.

Output stays **byte-identical** to the proven M1b/M2a blocking path under both Fifo and a non-Fifo
present mode. The M1b blocking path (`RIVE_BLOCKING=1`) and the fixed-ring fallback
(`RIVE_NO_WATERMARK=1`, or any device without timeline semaphores) are retained as fallbacks and A/B
baselines on one build.

All numbers are measured on the **native RTX 4090** relay (`scripts/win.cmd`, driver 591.86,
`WGPU_BACKEND=vulkan`), **release** rive libs, `octopus_loop.riv` at 512×512 into the shared `VkImage`,
n=300 frames after a 30-frame warm-up. Every figure traces to a raw artifact:
[docs/perf/m2b_step1_raw.txt](perf/m2b_step1_raw.txt) (correctness) and
[docs/perf/m2b_step2_raw.txt](perf/m2b_step2_raw.txt) (flush + run-ahead). Linux build stays green;
clippy clean; M1a CPU-copy floor unaffected.

---

## Step 1 — robust recycling watermark (correctness; removes the present-mode precondition)

### Design (verified against the real wgpu 27.0.1 / wgpu-hal 27.0.4 / Bevy 0.18.1 source)

Three candidate mechanisms were checked against the vendored source before any code:

- **Safe-wgpu completion watermark — INFEASIBLE.** `wgpu::SubmissionIndex` is opaque (`pub(crate)`,
  derives only `Debug,Clone` — not even `Ord`); `Device::poll(PollType::Poll)` returns a contentless
  `PollStatus` with no index; there is no public "last completed submission" getter. The only safe
  primitive, `Queue::on_submitted_work_done`, fires no index and **requires a poll to run** — and Bevy
  **never polls the device on native** (confirmed: `gpu_readback` relies on a later-frame `try_recv`,
  and the graph submit at `graph_runner.rs:87` drops its `SubmissionIndex`).
- **Own Vulkan timeline semaphore — FEASIBLE, chosen.** Fully public API. wgpu-hal's `Queue::submit`
  drains a per-queue signal list (`SemaphoreList::append`) into the frame's single `vkQueueSubmit`
  and threads timeline values via `VkTimelineSemaphoreSubmitInfo` — exactly as it signals its own
  fence. The render graph records every node then submits **exactly once** per frame, so a signal
  pushed from the node rides that frame's submit.

Implementation (all in [zero_copy.rs](../crates/bevy-rive/src/zero_copy.rs); the `safe_frame` FFI
plumbing already existed, so **no shim/C++ change**):

1. `extract_shared_handles_once` queries `VkPhysicalDeviceTimelineSemaphoreFeatures` via the device's
   `as_hal` Vulkan instance; if supported (and non-blocking), it creates one timeline semaphore
   (initial value 0) and stores the handle on `RiveSharedHandles`. Else `0` → fixed-ring fallback.
2. Each frame the node reads the exact watermark non-blocking via
   `Queue`/`Device` `as_hal` → `vkGetSemaphoreCounterValue`, and sets `safe_frame = completed`.
3. At end of frame the node arms the next signal:
   `render_queue.as_hal::<Vk>().add_signal_semaphore(sema, Some(frame_no))` — rides that frame's
   single graph submit, so the timeline reaches `frame_no` exactly when the frame's rive work
   completes on the GPU.

The semaphore is created once (device-lifetime) and **intentionally never destroyed** — destroying it
at drop would reintroduce the teardown-ordering / drop-time device access the M1b close-out removed.

**Safety argument (airtight):** rive recycles a transient buffer iff `lastFrameNumber ≤ safe_frame`.
A buffer last touched at frame K rode frame K's graph submit, and we signal the timeline to K *on that
same submit*, so the counter reaches K only once K's GPU work has completed. Therefore every frame
`≤ safe_frame` is provably GPU-done. The present-mode precondition is gone; the one-shot warn-guard now
fires only in the fixed-ring fallback.

### DoD — byte-identical under default AND a non-Fifo present mode

Frozen pose (`RIVE_SPEED=0`), transparent clear (`RIVE_CLEAR_ALPHA=0` → genuine `a<1`: AA edges + glow
halo). All three are md5-identical (`58c894e0…`, 90345 bytes — matching the M2a transparent reference):

| capture | sync path | md5 |
|---|---|---|
| blocking (M1b reference) | `blocking (M1b submit+fence)` | `58c894e0…` |
| watermark, **Fifo** | `timeline-semaphore watermark (M2b, exact)` | `58c894e0…` |
| watermark, **Immediate** (non-Fifo, CPU runs ahead) | `timeline-semaphore watermark (M2b, exact)` | `58c894e0…` |

The Immediate case is the one M2a's fixed offset could not guarantee; the octopus + semi-transparent
glow renders coherently (visually confirmed). **DoD met.**

---

## Step 2 — high-N flush characterization (measured before prescribing a fix)

A/B on **one build** under Fifo (the M2a measurement condition): the M2b exact watermark vs. the M2a
fixed offset (`RIVE_NO_WATERMARK=1`). A new per-frame **run-ahead** column (`current_frame − safe_frame`
= frames submitted but not yet GPU-complete) makes the mechanism observable. p50 µs, two passes where
taken; the `fixed` numbers reproduce the committed M2a range, validating the A/B:

| N | rive flush — **watermark** | rive flush — fixed (≈M2a) | flush Δ | frame CPU wm | frame CPU fixed | run-ahead wm | run-ahead fixed |
|--:|--:|--:|--:|--:|--:|--:|--:|
| 1 | 97 | 92 | ~0 | 139 | 137 | **1.0** | 3.0 |
| 8 | 329 | 421 | **−22%** | 514 | 609 | 1.0 | 3.0 |
| 32 | 986 / 1280 | 1994 / 2003 | **−36…−51%** | 1514 / 1912 | 2498 / 2524 | 1.0 | 3.0 |
| 128 | 4797 / 4778 | 6524 / 6554 | **−27%** | 6538 / 6455 | 8312 / 8344 | 1.0–2.0 | 3.0 |

Watermark also tightens the tail at N=32 (max 2410 / 2835 vs fixed 4745 / 5228).

**Run-ahead under a present mode that *permits* it** (Immediate, watermark — no vsync throttle):

| N | rive flush (Immediate) | run-ahead p50 (max) |
|--:|--:|--:|
| 1 | 33 | 1.0 (2) |
| 32 | 683 | 2.0 (2) |
| 128 | 6362 | 2.0 (2) |

### Conclusion — which hypothesis the data supports

**The "run-ahead / working-set" hypothesis is confirmed, with a refinement.** The fixed `current−3`
offset told rive it was *always 3 frames behind*, so rive kept ~3 frames of transient (host-mapped)
buffers live. The exact watermark reveals the GPU actually keeps up — **run-ahead ≈ 1** under Fifo —
so rive recycles a frame's buffers ~1 frame back, shrinking the live working set and cutting flush
(~half at N=32, ~27% at N=128). Flush tracks run-ahead directly: Immediate (run-ahead 2) sits between
Fifo-watermark (1) and fixed (3). So a meaningful share of M2a's "flush growth at high N" was an
artifact of the over-stated fixed offset, not intrinsic rive cost.

Crucially, **run-ahead self-bounds to ≤2 even under Immediate** (no vsync throttle): rive's transient
buffer pool back-pressures the CPU — with the watermark naming only the truly-free frames, rive's
`flush` waits for a safe buffer rather than racing unbounded. So the watermark both makes non-Fifo
**safe** and keeps the working set **small**. No `VK_ERROR` / device-lost / corruption in any run.

**The high-N bottleneck is now bounded — lower than M2a.** The residual N=128 flush (~4.8 ms, p99 tail
~2.3× p50) is genuine rive work (128 instances sharing one Context, frame-to-frame allocation jitter),
in the GPU-leaning regime where clockwise (M2 remainder) is the lever — not a recycling artifact.

---

## Step 3 — minimal fix (only if Step 2 implicates a specific cause)

**No fix applied.** Per the DoD, the Step 1 watermark already tames the high-N flush (≈halved at N=32,
−27% at N=128) and bounds run-ahead. The data is fully explained by the working-set mechanism; it does
**not** implicate shared-Context contention, so — per the milestone's explicit guard — no per-instance
contexts were built. The remaining N=128 cost/variance is intrinsic and lives in the GPU-leaning regime.

---

## Carry-over verification

**Vulkan validation layers were NOT enabled, and are not attestable in this environment — an honest
negative finding.** Bevy sets instance flags via `InstanceFlags::default().with_env()`, and
`default()` → `from_build_config()` returns only `VALIDATION_INDIRECT_CALL` in **release** (no
`VALIDATION`). Forcing `WGPU_VALIDATION=1` then logs
`WARN wgpu_hal: InstanceFlags::VALIDATION requested, but unable to find layer:
VK_LAYER_KHRONOS_validation` — the Khronos validation layer is installed on neither the Windows relay
(no `VULKAN_SDK`) nor WSL2 (only Mesa layers). **So the "0 validation errors" noted in M1b/M2a/M2b runs
only ever attested "no device-lost / no crash," not the absence of layout/sync VUID violations.**

The load-bearing **layout-handoff invariant** is instead verified by code + indirect evidence: the
shim's record path explicitly records a barrier to `VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL`
([rive_shim.cpp:1354](../crates/rive-renderer-sys/shim/rive_shim.cpp)) into wgpu's command buffer —
which equals the `RESOURCE` layout wgpu's tracker expects when it samples the shared texture, so wgpu
emits no corrective barrier. If this were wrong, sampling would read a wrong-layout image (garbled or a
validation error); the byte-identical output across blocking / non-blocking / watermark and Fifo /
Immediate, with zero device-lost over thousands of frames, is strong empirical corroboration.

**Follow-up (recommended):** install `VK_LAYER_KHRONOS_validation` (`apt install vulkan-validationlayers`
→ run on Dozen, or the Vulkan SDK on Windows) and re-run with `WGPU_VALIDATION=1` for direct
layer attestation of the handoff. Load-bearing for Mesa / future D3D12 / Metal backends.

---

## Adversarial review

Per the house cadence ([workflow-cadence]), a milestone ends with one adversarial review. The
multi-agent cloud workflow requires explicit opt-in (not given for M2b), so this was a focused
**self-review** across the review dimensions (sync-correctness, unsafe/FFI, resource-leak,
perf-honesty, API-regression). Two findings, both fixed + re-verified:

1. **(perf-honesty)** The run-ahead metric mis-reported the blocking path (the hoisted `safe_frame` fell
   to `frame−3` while the blocking submit used `frame−1`). Unified `safe_frame` across all three paths
   (blocking / watermark / fallback) so the metric is accurate everywhere; byte-identical output
   re-confirmed (`58c894e0…`) — a no-op for the rendered result.
2. **(doc)** Documented the timeline semaphore as an intentional device-lifetime leak (no drop-time
   `vkDestroySemaphore`, to avoid the teardown hazard M1b removed).

Core sync-safety, the `as_hal` unsafe blocks (guards never stored, SAFETY comments present), and
API-regression (frozen M1a/M2a API untouched; new knobs additive) reviewed sound. *A full multi-agent
workflow review can still be run on request.*

---

## What's next (M2 remainder)

- **Clockwise re-eval** — the pipeline goes GPU-leaning at N≥8 (M2a) and the watermark leaves rive's
  GPU commands unchanged, so clockwise's ~23–28% GPU saving is the next lever now that the CPU regime
  is bounded.
- **Direct validation-layer attestation** of the layout handoff (above).
- M1a-floor in-place upload; pipelining return only with a sound cross-thread *drop* strategy;
  native-Linux NVIDIA validation when a bare-metal env exists.

New/changed knobs: `RIVE_PRESENT_MODE=fifo|immediate|mailbox|fifo_relaxed` (example) and
`RIVE_NO_WATERMARK=1` (force the fixed-ring fallback for A/B).
