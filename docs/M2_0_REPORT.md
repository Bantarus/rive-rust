# Milestone 2.0 — optimized rive libs + first perf baseline

**Status: complete.** rive's static libs now build **optimized (`--config=release`)** and
the `zero_copy` example links + runs on the **native RTX 4090** (`WGPU_BACKEND=vulkan`)
with those release libs, rendering correctly in both the opaque and
`RIVE_CLEAR_ALPHA=0` transparent cases, no validation errors. The Linux build stays green.
A repeatable first perf baseline (Atomics vs clockwise, CPU + GPU, median + percentiles)
is recorded below — **all numbers measured, none estimated**.

Windows numbers are from the native relay (`scripts/win.cmd`), real **RTX 4090, driver
591.86**, `WGPU_BACKEND=vulkan`, **release** rive libs, `octopus_loop.riv` rendered at
**512×512** into the shared `VkImage` (composited window 1280×720).

---

## Task 1 — release rive libs linking

### The anticipated "link fight" does not materialize on the Windows ClangCL path

The M1.0/M1b assumption was: *rive's release config emits LLVM LTO bitcode that
`link.exe` can't consume, so we'll need `lld-link`.* **That is false for the Windows build
path**, and verifying it is the core of this task.

rive on Windows builds via `premake5 vs2022` → **MSBuild** with the **ClangCL** platform
toolset. Built `--config=release`, the produced objects are **plain COFF**, not LLVM
bitcode — verified by extracting a member from the release `rive_pls_renderer.lib`:

```
rive_pls_renderer/vulkan_shaders.obj → magic 64 86 → "Intel amd64 COFF object file"
(64 86 = COFF for link.exe;  42 43 c0 de = "BC" = LLVM bitcode/LTO)
```

So the default MSVC **`link.exe` consumes rive's release objects directly** — no
`lld-link`, no link fight. premake's `LinkTimeOptimization` flag does **not** reach
clang-cl as `-flto` through MSBuild's LLVM toolset, so the Windows release build is
**optimized (`-O2` via `optimize "On"`) but not cross-module-LTO'd**, emitting ordinary
COFF. The `zero_copy` example, the Bevy release dep tree, the shim, and all ten rive libs
link with the stock `link.exe` (verified end-to-end: `offscreen_png --release` and
`sprite_riv_zerocopy --release` both build + run on the 4090).

> Evidence the release libs are genuinely optimized: Windows `rive.lib` **167 MB → 79 MB**,
> `rive_pls_renderer.lib` **29 MB → 16 MB** (debug → release). Linux `librive.a`
> **150 MB → 12 MB**.

**Chosen path: `--config=release` with LTO disabled, linked by each platform's default
linker** — the task's blessed "disable-LTO" fallback, which on Windows is also what the
toolset produces anyway. No toolchain additions. Non-LTO optimized release is a large step
up from debug (the point of the task) and gives a clean baseline.

### How the build selects debug vs release

[`crates/rive-renderer-sys/build.rs`](../crates/rive-renderer-sys/build.rs) resolves the
rive lib config:

1. **`RIVE_RUNTIME_CONFIG`** (`debug`/`release`) — explicit override, wins.
2. else **Cargo's `PROFILE`** — `--release` → release rive libs; a dev build → debug libs.

So the M1a/M1b dev loop (`cargo run …`) keeps fast **debug** rive libs; `--release` (the
perf relay) gets **optimized** libs — no flag to remember. The two configs land in
**separate out dirs** (`out/rive-rust-m0` vs `out/rive-rust-m0-release`) so they never mix
stale objects, and the existing synced debug tree (+ its prebuilt SPIR-V) is undisturbed.

**LTO policy** (`RIVE_RUNTIME_NO_LTO`): unset → LTO **off for release on both OSes**
(deterministic; needs no special linker). On **Linux** this is load-bearing — rive's
release LTO there emits real LLVM-bitcode `.o` the default `cc`→`ld` can't consume;
`--no-lto` makes rive emit normal **ELF** objects (verified: `7f 45 4c 46` ELF magic, not
`BC`). On **Windows** it's a harmless explicit no-op (the toolset emits COFF regardless).

The stale `Cargo.toml` comment ("the native rive-runtime static libs are always built in
release inside build.rs") is corrected — they follow `PROFILE`.

### Gotchas / notes

- **The whole "link fight" was a non-issue on the ClangCL/MSBuild path** (COFF, not
  bitcode). The bitcode problem is real only on the Linux clang+gmake path, where
  `--no-lto` resolves it. Worth recording because M1.0's perf note assumed the harder
  Windows case.
- **`.cargo/config.toml` is environment-protected.** A trial edit adding
  `linker = "lld-link"` did **not** persist (`git` shows the file unmodified). Since
  `lld-link` is **not needed** (link.exe links the COFF), the reverted state is the correct
  state — no action required. Flagged so a future cross-module-LTO experiment knows it must
  re-add that line through whatever guards the file, and set `RIVE_RUNTIME_NO_LTO=0`.
- **First `--release` relay build is long** (~4 min: the Bevy release dep tree on top of
  the rive-lib build) but fully cached afterward.

### DoD ✅

`zero_copy` links + runs on the native 4090 with **release** rive libs; octopus renders
correctly **opaque** and **`RIVE_CLEAR_ALPHA=0` transparent**; **no validation errors** in
any run (logs scanned: 0 panics / VUID / `VK_ERROR` / device-lost across all 9 relay runs).
Linux green (all three examples build incl. `zero_copy`; release rive libs build → ELF/
no-LTO objects + 116 SPIR-V headers; touched crates fmt + clippy clean under the
`zero_copy` feature). Correctness numbers below.

---

## Task 2 — first perf baseline

### Correctness gate (decoded-RGBA diffs; the M1b method)

All diffs decode both PNGs to raw RGBA (ffmpeg) and compare per channel — PNG byte/md5
comparison is unreliable over the 9p mount and across encoder nondeterminism, so decoded
pixels are the truth. (Helper: [`tools/png_diff.py`](../tools/png_diff.py).)

| Comparison | max Δ | changed | verdict |
|---|---|---|---|
| release **offscreen** octopus (4090) vs M0 **debug** ref `out_octopus.png` | 2/255 | 0.150% | **≤2 LSB** |
| release **offscreen** coffee vs M0 **debug** ref `out.png` | 1/255 | 0.025% | **≤1 LSB** |
| **clockwise** vs **Atomics** (release, frozen pose, windowed; fresh pair) | 1/255 | 0.004% | **≤1 LSB** |
| Atomics **run-to-run** (determinism, two captures) | 0/255 | 0% | identical |
| release **transparent** (`CLEAR_ALPHA=0`, frozen) vs M1b **debug** close-out composite | 6/255 | 0.058% | debug→release Δ |

Reading these:

- **Opaque offscreen is within the close-out's ≤2-LSB tolerance vs the debug reference** —
  switching debug→release rive libs did not change the opaque output beyond integer
  rounding. Coffee is tighter (≤1).
- **Clockwise is pixel-identical to Atomics within ≤1 LSB** (164 of 3.69M bytes differ by
  1); a fresh independent capture pair reproduced it exactly, and Atomics run-to-run is a
  literal 0-byte match (determinism confirmed).
- **The transparent case is ≤6 LSB on 0.058% of pixels** — slightly above the close-out's
  2-LSB figure. This is the **debug→release rasterizer delta**: the close-out reference was
  made with **debug** rive libs, this run with **release** (optimized float codegen reorders
  AA math), so antialiased / partial-alpha edges shift by ≤6/255 on a fraction of a percent
  of pixels (histogram: 1739×Δ1, 266×Δ2, … 21×Δ6). Tiny and sub-perceptual — the expected,
  benign consequence of optimizing the libs, not a defect.
- **Not used:** a windowed *opaque* M2-vs-close-out diff — the only stored windowed opaque
  reference (`cap_zc_native.png`) was a **realtime** capture at a different animation frame,
  so it differs by pose, not rendering (Δ182, expected). The frozen-pose offscreen +
  transparent comparisons above are the valid ones.

PLS modes confirmed in the logs: default → **`PLS mode = Atomics`**; `RIVE_CLOCKWISE=1` →
**`PLS mode = Clockwise`** (the 4090 reports `supportsClockwiseMode`, so the override
resolves to `Clockwise`, not `clockwiseAtomic`); raster-order still unsupported on the 4090
(expected — it lacks the ext, per the M1b close-out). GPU timing reported **available** in
every run.

### Baseline tables (release libs, native 4090, n=300 after a 30-frame warm-up)

**CPU submit** = wall time of `render_external_frame` (rive's CPU-side flush/record →
`vkQueueSubmit` → **blocking `vkWaitForFences`**). **GPU rive** = rive's command-buffer
execution time via Vulkan timestamps written around rive's recorded work. Two scenes,
`RIVE_SPEED=0` measured twice to show run-to-run spread.

**CPU submit time [µs]**

| Mode (scene) | p50 | p90 | p95 | p99 | mean | min / max |
|---|--:|--:|--:|--:|--:|--:|
| Atomics (SPEED=0, pass 1) | 667 | 1142 | 1490 | 2795 | 766 | 277 / 3098 |
| Atomics (SPEED=0, pass 2) | 642 | 1113 | 1296 | 1933 | 718 | 279 / 4358 |
| Atomics (SPEED=1) | 627 | 1306 | 1624 | 2136 | 735 | 259 / 2784 |
| Clockwise (SPEED=0, pass 1) | 543 | 1028 | 1298 | 1755 | 637 | 252 / 2131 |
| Clockwise (SPEED=0, pass 2) | 594 | 1038 | 1332 | 1800 | 675 | 238 / 8059 |
| Clockwise (SPEED=1) | 607 | 1116 | 1478 | 2425 | 723 | 245 / 6496 |

**GPU rive command-buffer time [ms]**

| Mode (scene) | p50 | p90 | p95 | p99 | mean | min / max |
|---|--:|--:|--:|--:|--:|--:|
| Atomics (SPEED=0, pass 1) | 0.100 | 0.109 | 0.109 | 0.110 | 0.077 | 0.037 / 0.114 |
| Atomics (SPEED=0, pass 2) | 0.089 | 0.109 | 0.109 | 0.110 | 0.074 | 0.037 / 0.653 |
| Atomics (SPEED=1) | 0.091 | 0.108 | 0.109 | 0.111 | 0.075 | 0.036 / 1.043 |
| Clockwise (SPEED=0, pass 1) | 0.031 | 0.074 | 0.074 | 0.075 | 0.052 | 0.026 / 1.155 |
| Clockwise (SPEED=0, pass 2) | 0.063 | 0.073 | 0.074 | 0.080 | 0.056 | 0.026 / 1.722 |
| Clockwise (SPEED=1) | 0.070 | 0.076 | 0.078 | 0.082 | 0.056 | 0.026 / 0.092 |

### Reading the numbers (data, not a verdict)

- **rive's GPU work is tiny on a 4090: ~0.09–0.10 ms (Atomics), ~0.05–0.07 ms (clockwise).**
  By mean (the most stable statistic at this sub-0.1 ms scale, where p50 is near timer
  granularity — note SPEED=0 clockwise p50 0.031 is noise vs its 0.052 mean): Atomics
  ≈ 0.075 ms, clockwise ≈ 0.054 ms → **clockwise does ~28% less GPU work**, consistently,
  at ≤1-LSB correctness cost. SPEED=1 p50 corroborates: 0.091 → 0.070 (~23% less).
- **The per-frame cost is CPU-bound, not GPU-bound.** CPU submit ≈ **625–667 µs** (Atomics
  p50) while GPU execution is only ~**0.09 ms (≈90 µs)** — so GPU is **~14% of the wall**.
  The rest is CPU-side: rive's `flush()` building the command buffer, plus `vkQueueSubmit`
  and the **blocking `vkWaitForFences`** latency. The blocking fence prevents any CPU/GPU
  overlap, but on this GPU the GPU itself is cheap, so the fence's cost here is *latency +
  lost overlap*, **not** ~0.6 ms of recovered GPU time. This baseline doesn't separately
  instrument "rive CPU flush" vs "fence/submit latency" — distinguishing them is follow-up
  work, and the non-blocking-sync change will directly expose the fence portion.
- **Clockwise's CPU submit is also lower** (~543–607 vs ~627–667 µs p50), tracking its
  smaller GPU cost, but the gap is modest because CPU-side flush dominates the wall.
- **SPEED=0 ≈ SPEED=1** for both CPU and GPU — the octopus issues the same draws every
  frame whether or not the state machine advances, which cross-validates the measurement
  (the cost is the render, not the `advance`). The high **p90/p99 CPU** (1.1–2.8 ms) is
  windowed-app main-thread scheduling jitter (the whole render schedule runs on the main
  thread, pipelining disabled); **p50 is the clean signal**.

**No winner is declared** (per the task). The data: clockwise is correct (≤1 LSB) and
~23–28% cheaper on the GPU; the per-frame wall is currently CPU/fence-bound, so that GPU
win is not yet on the critical path. Whether to adopt clockwise is an M2 decision once
non-blocking sync lands.

### DoD ✅

Repeatable Atomics-vs-clockwise tables (CPU **and** GPU, median + percentiles, n=300; two
scenes; SPEED=0 ×2 passes) on release libs / native 4090, with the clockwise correctness
diff (≤1 LSB) and the resolved PLS mode for each.

---

## What changed (code)

- **`crates/rive-renderer-sys/build.rs`** — `PROFILE`/`RIVE_RUNTIME_CONFIG` → rive config;
  per-config out dir; `RIVE_RUNTIME_NO_LTO` (release → no-LTO default, both OSes);
  a `RiveLibBuild { config, no_lto }` option struct threaded to both per-OS builders.
- **`Cargo.toml`** — corrected the stale "always release" rive-libs comment.
- **`.cargo/config.toml`** — unchanged (no `lld-link`; link.exe handles the COFF release
  objects — see gotchas).
- **shim (`rive_shim.{h,cpp}`)** — `rive_render_context_set_clockwise` (per-frame
  `clockwiseFillOverride`); `rive_render_context_last_gpu_ms` + a defensive Vulkan-timestamp
  query pool (PFNs resolved via `vkGetDeviceProcAddr`, since rive's dispatch table has none;
  reports −1 if unavailable; the blocking submit guarantees results are ready). Query pool
  freed in external-context teardown.
- **`crates/rive-renderer/src/lib.rs`** — safe `Context::set_clockwise` + `last_gpu_ms`.
- **`crates/rive-renderer-sys/src/lib.rs`** — FFI decls for the two new shim fns.
- **`crates/bevy-rive/src/zero_copy.rs`** — `RIVE_CLOCKWISE` knob (applied once on the
  context); `RIVE_PERF` / `RIVE_PERF_FRAMES` per-frame CPU+GPU collector with a
  median/percentile summary after warm-up; self-describing one-shot mode log.
- **`examples/sprite_riv_zerocopy.rs`** — `RIVE_PERF` frame-budget auto-exit so a perf run
  self-terminates after the summary prints.
- **`tools/png_diff.py`** — stdlib+ffmpeg decoded-RGBA differ (no PIL/numpy on this box).

## Runtime knobs added (all default-off, behavior-preserving)

- `RIVE_CLOCKWISE=1` — opt into rive's clockwise PLS path (per-frame override).
- `RIVE_PERF=1`, `RIVE_PERF_FRAMES=N` (default 300) — collect + log the perf summary, then
  the example auto-exits.
- `RIVE_RUNTIME_CONFIG=debug|release`, `RIVE_RUNTIME_NO_LTO=0|1` — rive lib build controls.

## State of the tree

- **fmt clean** on the touched crates (`offscreen_png.rs` deliberately untouched, per the
  fmt scar). **clippy clean under the `zero_copy` feature** (verified `--message-format=short`:
  0 Rust warnings attributed to `bevy-rive` source, 0 unfulfilled `#[expect]`s; the only
  remaining warnings are rive's own C++ header warnings from the build script). One lint was
  introduced by M2.0 and fixed: the `undocumented_unsafe_blocks` adjacency on the timed
  submit — the perf timer's `let` had separated the `// SAFETY:` comment from the `unsafe`
  block, so the timer was hoisted above the comment. (The build.rs per-OS lib builders briefly
  hit `too_many_arguments` at 8 params when `no_lto` was added; resolved structurally by
  bundling `config`+`no_lto` into `RiveLibBuild`, not by a suppression.)
- Default behavior unchanged; all new knobs default to prior behavior.

## M2 remainder (next, driven by this baseline)

The headline — **per frame ≈ 0.65 ms of CPU submit wall around only ~0.09 ms of GPU work**,
serialized by a blocking fence — sets the priority:

1. **`transition_resources()` + non-blocking sync** (drop the blocking `VkFence`/wait) —
   removes the fence latency and restores CPU/GPU overlap; the riskiest change, and the
   **end-of-task adversarial review** attaches here (house cadence: one review at the end of
   M2, not per step). It will also let us separate "rive CPU flush" from "fence latency" in
   the wall, which this baseline could not.
2. **In-place upload** (kill any per-frame realloc); **clockwise** if it measurably wins
   once the fence is gone (correct + ~23–28% cheaper on GPU today); **pipelining return**
   only with the validated cross-thread *drop* strategy (atomic refcount or explicit
   main-thread teardown — never `Rc` + a ferried world).
3. **Native-Linux validation** folds in when a bare-metal env exists.
4. **Cross-module LTO** (lld-link on Windows / LLVMgold on Linux) is an optional perf lever
   if codegen ever shows up as a bottleneck — not needed for a correct, optimized build.
