# Milestone 1b — close-out (remove `unsafe`, close verifications)

Follow-up to [docs/M1B_REPORT.md](M1B_REPORT.md). Four tasks: remove the
load-bearing `unsafe`, close the PLS diagnosis, prove the colorspace resolution on
transparent content, and check native Linux. All validated on the **native Windows
relay (real RTX 4090, driver 591.86, `WGPU_BACKEND=vulkan`)** and **WSL2-via-Dozen**.
Correctness only — rive libs are still debug; no perf claims, no perf tuning.

---

## Task 1 — the load-bearing `unsafe` is gone

**Removed**, and not by suppression — by eliminating the condition that required it.

- **Pipelined rendering disabled for the tier.** The `sprite_riv_zerocopy` example
  builds `DefaultPlugins.disable::<PipelinedRenderingPlugin>()`. The render world
  (which owns rive's `!Send` handles) now runs on the **main thread**.
- **`RiveRenderState` is a `NonSend` render-world resource** (`init_non_send_resource`,
  read in the node via `world.get_non_send_resource()`). `NonSend` requires neither
  `Send` nor `Sync`, so the hand-written **`unsafe impl Send + Sync` is deleted**.
- **Wrapper reverted `Arc → Rc`** (it was `Arc` only for a sound cross-thread *drop*
  under pipelining; single-threaded now, so non-atomic `Rc` is sound) and the
  **crate-wide `arc_with_non_send_sync` clippy suppression is removed**.
- A plugin-build guard logs a loud error if `PipelinedRenderingPlugin` is still
  present (we cannot remove an already-added plugin from within a plugin).

This is **elimination, not suppression**: with the render world single-threaded
there is no cross-thread drop for `Rc` to be unsound against, and no cross-thread
move forcing a `Send` assertion. The trade-off is the loss of main/render overlap —
acceptable for a correctness-first, debug-lib tier, and reversible.

**M2 breadcrumb** (baked into the rive-renderer crate doc): if pipelining returns,
the hazard is the **drop** thread, not the use thread — Bevy decides when/where a
ferried `World` tears down. Making the move `Send` is not enough; the refcount
decrement must be made sound too (atomic `Arc`, or explicit main-thread teardown).
**Do not pair a non-atomic `Rc` with a ferried world.**

**DoD ✅** No `unsafe impl` and no `arc_with_non_send_sync` suppression remain in the
rive path (`grep` clean). `zero_copy` renders the octopus correctly on the native
4090 **and** Dozen, no validation errors. M1a CPU-copy floor unaffected (the M1a
example + workspace build green; wrapper unit tests pass).

> **Empirical note (the watch-point):** the worry was that the render-graph node
> might execute off the NonSend's origin thread. It does not — with pipelining off
> the node runs on the main thread, no off-origin-thread panic on either backend.
> The unsafe-free fallback (move advance+submit into a NonSend *system*, leave only
> the display pass in the node) was **not needed**.

---

## Task 2 — PLS diagnosis closed (the answer is neither hypothesis)

The experiment: also enable `VK_EXT_rasterization_order_attachment_access` + chain
its feature, instrument the callback to log the **final pNext chain** wgpu hands to
`vkCreateDevice`, and re-measure on native.

**Measured (native 4090, shipping defaults):** `PLS mode = Atomics, raster-order
supported = false`. Octopus renders, no validation errors.

**pNext evidence (logged in the device-create callback):**
```
device-create callback pushed interlock exts (raster=false, pixel=true);
final pNext sType chain wgpu will pass to vkCreateDevice =
    [PHYSICAL_DEVICE_FRAGMENT_SHADER_INTERLOCK_FEATURES_EXT]
extract: rasterization_order_color_attachment_access=false,
         fragment_shader_pixel_interlock=true
```

**Interpretation — both original hypotheses are refuted; the real cause is hardware:**

1. **NOT a chain-survival bug.** The final pNext chain wgpu passes contains exactly
   the struct we pushed (`FRAGMENT_SHADER_INTERLOCK_FEATURES_EXT`). Our chained
   feature **survives `open_with_callback` intact** — nothing is dropped.
2. **NOT simply the "wrong extension we forgot."** The callback now *tries* to enable
   `VK_EXT_rasterization_order_attachment_access`, but its own
   `enumerate_device_extension_properties` check returns **`raster=false`**: the
   **NVIDIA RTX 4090 (591.86) does not advertise that extension at all** (it is an
   ARM/AMD/tiler-class extension; NVIDIA desktop exposes `fragment_shader_interlock`
   instead). So rive's `supportsRasterOrderingMode` — which keys off
   `rasterizationOrderColorAttachmentAccess` (render_context_vulkan_impl.cpp:960) —
   is correctly `false`.
3. **Atomics is rive's correct fallback here, not a bug.** rive's
   `select_interlock_mode` (render_context.cpp:348) picks `clockwise` **only** when
   the per-frame `clockwiseFillOverride` is set (an opt-in we don't request), and
   `rasterOrdering` only when `supportsRasterOrderingMode`. With neither, it falls
   through to `atomics` — even though `fragment_shader_interlock` makes clockwise
   *available*. So the persistent Atomics is **expected** for a desktop NVIDIA GPU on
   this code path; it does not indicate a broken feature chain.

Two diagnostic fixes landed alongside:
- **PLS-log race fixed.** The one-shot mode log is now gated on an actually-rendered
  frame (`rendered_any`), because `frameInterlockMode()` is only meaningful after a
  `beginFrame`; previously it could log `Unknown` on an early empty node call.
- **Optimistic log reworded.** The old "expecting raster-order PLS" (keyed on the
  wrong extension) now reports the two features separately and which one actually
  yields raster-ordering.

**Follow-up (perf tier, M2 — does NOT block the zero-copy DoD):** rive's faster path
on NVIDIA is **clockwise** PLS, reachable only via `clockwiseFillOverride` per frame
(not a capability we toggle at device creation). Whether to opt into it is a
perf/quality decision deferred to M2 with optimized libs. Atomics renders correctly.

**DoD ✅** Measured mode + interpretation + pNext evidence reported above.

---

## Task 3 — transparent-alpha resolution proven (measured, not asserted)

The octopus over the default **opaque** clear has **0 partial-alpha pixels** (rive
composites everything onto the opaque dark gray), so it only exercises the trivial
`a==1` case. To genuinely test the un-premultiply `c/a` divide, a shared
**`RIVE_CLEAR_ALPHA`** test knob (default `1.0`, behavior-preserving) clears rive to
**transparent** so antialiased edges + the soft glow become partial-alpha.

- **Content:** with `RIVE_CLEAR_ALPHA=0` the octopus offscreen has **43,352 /
  262,144 = 16.5% partial-alpha pixels** (`0 < a < 255`).
- **Method:** render the same static pose (`RIVE_SPEED=0`, another knob, so the
  state machine is frozen and the M1a/M1b poses match exactly) through M1a (CPU-copy)
  and M1b (zero-copy) on the same backend (Dozen / real 4090); diff the composites.
- **Result (1280×720 RGBA):** **max per-channel delta = 2 / 255**, mean ≈ 0.0003,
  only 960 of 3.69M bytes differ at all (0.026%), 2 bytes by 2 LSB, none ≥ 4.
- **Tolerance:** ≤ 2 LSB per channel — exactly the expected difference between M1a's
  integer `round(c·255/a)` and M1b's float `c/a` + sRGB store round-trip.

This confirms the in-shader order (un-premultiply in encoded space → sRGB-decode →
straight, re-encoded by the `Rgba8UnormSrgb` target on store) matches M1a
pixel-for-pixel within rounding, **including `a < 1`**. (At the default opaque clear
the two paths are byte-identical.)

A transparent reference is saved locally as `out_octopus_transparent.png` (gitignored,
alongside the other `out*.png` artifacts).

**DoD ✅** Measured transparent-content diff, tolerance documented.

---

## Task 4 — native Linux NVIDIA: DEFERRED (validation gap)

This is a **WSL2** box, which has no functional native NVIDIA Linux Vulkan ICD:
pinning `nvidia_icd.json` fails at instance creation —
`loader_scanned_icd_add: Could not get 'vkCreateInstance' … for ICD libGLX_nvidia.so.0`
(WSL2 routes the GPU via `/dev/dxg` → D3D12/Dozen, not the bare-metal driver). No
bare-metal Linux env is at hand, so this is **deferred**.

**Risk: low.** The Vulkan path is shared with the native Windows relay, which *is*
validated on the real 4090 (same rive Vulkan shim, same FFI, same device-sharing
callback). Native Linux would only additionally exercise the Linux Vulkan loader +
driver combo. It remains the one open validation gap for M1b.

---

## State of the tree

- **fmt + clippy clean** on our crates (bevy-rive, rive-renderer, rive-renderer-sys).
  Pre-existing, committed rustfmt drift in `build.rs` / the M0 `offscreen_png.rs`
  example is intentionally left untouched (outside M1b, proven-working on the relay).
- Default behavior unchanged: opaque clear, realtime speed. The `RIVE_CLEAR_ALPHA`
  and `RIVE_SPEED` knobs default to the prior behavior; they are test affordances.

## M2 (next)

Optimized rive libs (→ real perf numbers), `transition_resources()` + non-blocking
sync (drop the blocking fence), in-place upload, and — if perf wants the overlap
back — re-enable pipelining **with** a validated cross-thread *drop* strategy (per
the breadcrumb above). Native-Linux validation (Task 4) folds in when an env exists.
The single end-of-task adversarial review runs in M2 per house cadence.
