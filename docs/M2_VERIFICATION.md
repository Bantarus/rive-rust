# M2 verification gate — validation-layer attestation + adversarial review of the sync interop

**Status: complete.** Attested on **Dozen** (atomic path) **and native NVIDIA RTX 4090** (the
authoritative ICD — both the default Atomics path and the Clockwise/interlock path).
A verification milestone — **no new features**. It closes the two gaps left by M2a/M2b: (1) the
zero-copy/sync interop had never run under Vulkan validation layers, and (2) the M2b sync rework got a
self-review, not a multi-agent adversarial pass.

Environment: WSL2 + Mesa **Dozen** (Vulkan→D3D12 on the real RTX 4090, atomic PLS path) for the immediate
attestation, **and the native RTX 4090 relay** (real NVIDIA Vulkan ICD, interlock extension enabled) for
the authoritative attestation across both the Atomics and Clockwise/interlock PLS paths. Linux build green.

---

## Task 1 — validation-layer attestation

### Setup (validation was genuinely off before)

Bevy sets instance flags via `InstanceFlags::default().with_env()`, and `default()` →
`from_build_config()` drops `VALIDATION` in **release** — so every prior "0 validation errors" (M1b /
M2a / M2b) attested only "no device-lost / no crash," **never** the absence of layout/sync VUIDs. For
this gate: the Khronos validation layer is installed (`vulkan-validationlayers`), runs are a **debug**
build with `WGPU_VALIDATION=1`, and **synchronization validation** is enabled via a layer-settings file
(`khronos_validation.validate_sync = true`, logged to stdout). Each run's log carries the layer banner
*"Khronos Validation Layer Active … Current Enables: VK_VALIDATION_FEATURE_ENABLE_SYNCHRONIZATION_VALIDATION"*
— so the attestation is not vacuous: the layer (core **and** sync) is provably engaged.

### Attestation matrix (Dozen, atomic path) — per-cell results in [m2v_attestation.txt](perf/m2v_attestation.txt)

Frozen pose, 30-frame capture, sync-validation confirmed engaged per cell. The **only** validation
error in any run is `VUID-vkCmdCopyBufferToImage-imageOffset-07738` — and it is **out of scope** (see
below); the table reports interop-relevant findings only.

| present mode | watermark mode | alpha | rendered | sync-val on | SYNC-HAZARDs | interop VUIDs |
|---|---|---|:--:|:--:|:--:|:--:|
| Fifo | timeline (M2b) | opaque | ✓ | ✓ | none | none |
| Fifo | timeline (M2b) | transparent | ✓ | ✓ | none | none |
| Fifo | fixed-ring | opaque | ✓ | ✓ | none | none |
| Fifo | fixed-ring | transparent | ✓ | ✓ | none | none |
| Immediate | timeline (M2b) | opaque | ✓ | ✓ | none | none |
| Immediate | timeline (M2b) | transparent | ✓ | ✓ | none | none |
| Immediate | fixed-ring | opaque | ✓ | ✓ | none | none |
| Immediate | fixed-ring | transparent | ✓ | ✓ | none | none |

**Scrutinized specifically (all clean):**
- The `as_hal` raw-command-buffer recording (rive's commands wgpu's tracker never sees) — no VUID.
- The `COLOR_ATTACHMENT → SHADER_READ_ONLY` handoff — **synchronization validation reports no
  write→read hazard**, so the shim's post-flush barrier ([rive_shim.cpp:1354](../crates/rive-renderer-sys/shim/rive_shim.cpp))
  is sufficient and the layout wgpu's tracker expects is the layout rive leaves.
- The timeline-semaphore signal/wait usage — no semaphore VUID (after the fix below).
- The image layout at the display-pass sample — no layout VUID.

### Native NVIDIA RTX 4090 attestation (the authoritative ICD)

Re-ran the matrix on the native relay (real NVIDIA Vulkan ICD, driver 591.86, **release** +
`WGPU_VALIDATION=1`, sync validation confirmed per cell) once the Windows Vulkan SDK was installed. Native
runs the real path with `VK_EXT_fragment_shader_interlock` **enabled** (Dozen lacks it). Added two
`RIVE_CLOCKWISE` cells (validation-only) so the **Clockwise/interlock PLS path** — which records
*different* (interlock) barriers than atomics — is exercised, not just the default atomics path. Full
per-cell results are in [m2v_attestation.txt](perf/m2v_attestation.txt); the near-identical per-cell raw
logs are slimmed to one representative per environment (`representative_native_atomics.log` +
`representative_native_interlock.log`) under [docs/perf/m2v_logs/](perf/m2v_logs/).

| path | cells | rendered | sync-val on | SYNC-HAZARDs | interop VUIDs |
|---|---|:--:|:--:|:--:|:--:|
| **Atomics** (default) — Fifo/Immediate × watermark/fixed × opaque/transparent | 8 | ✓ | ✓ | none | none |
| **Clockwise / interlock** (`RIVE_CLOCKWISE`) — Fifo × opaque/transparent | 2 | ✓ | ✓ | none | none |

**Zero interop VUIDs / SYNC-HAZARDs on native, on both PLS paths.** No `vkDestroyDevice` semaphore VUID
(the RiveFrameSync fix holds on the real ICD too); the Dozen cubemap VUID is **absent** (native transfer
granularity `(1,1,1)`). Byte-identical across sync modes: opaque `md5 1de62920` (×4), transparent
`58c894e0` (= the committed M2b reference). So the deferred native interlock-path gate is now **closed and
clean** — only a shader-codegen VUID remains (out of scope, below).

### VUID found and FIXED — the gate did its job

**`VUID-vkDestroyDevice-device-05137`** fired before the fix: *"VkSemaphore … has not been destroyed"*
at device teardown. M2b created the timeline semaphore and **deliberately never destroyed it** ("device
-lifetime leak"); the validation layer correctly flags that as a spec violation — and it is **universal,
not Dozen-specific** (it would fire on native NVIDIA too). This is exactly a "valid on NVIDIA, invalid
per spec" defect the gate exists to catch.

**Fix:** a `RiveFrameSync` resource now owns the semaphore and destroys it on teardown
(`device_wait_idle` then `destroy_semaphore`), holding a `wgpu::Device` **clone** so the `VkDevice`
stays alive across destruction **regardless of render-world resource drop order**. This is *not* the
M1b drop-time hazard (that was the `!Send` rive `Rc` state under pipelining; this is a plain `Send`
wgpu handle with pipelining off). Re-verified: the destroy-VUID is **gone**, and native output is
**byte-identical** to the committed M2b reference (`md5 58c894e0…`) — the fix only changed teardown.

### Out of scope — two non-interop VUIDs (one per environment)

- **`VUID-vkCmdCopyBufferToImage-imageOffset-07738`** (Dozen only, 54×/run): on **`(wgpu internal)
  PendingWrites`** copying a **1×1×6** image (a Bevy default cubemap), triggered by **Dozen reporting an
  invalid transfer granularity `(0,0,0)`**. wgpu/Bevy-internal, a Dozen non-conformance — absent on
  native (granularity `(1,1,1)`).
- **`VUID-StandaloneSpirv-None-10684`** (native only, 16×/run): strict `spirv-val` flags explicit-layout
  (`Offset`/`MatrixStride`) decorations on Function-storage variables in **naga-compiled shaders** (Bevy's
  matrix-bearing shaders). **Not our interop, and not our un-premult blit** — that WGSL has no
  struct/matrix/array in any function variable, so it cannot emit those decorations (verified by reading
  it). A pre-existing upstream wgpu/naga shader-codegen pattern the NVIDIA driver accepts (output is
  byte-identical-correct). Dozen's older layer didn't run full `spirv-val`, which is why only the native
  SDK surfaced it.

Both are shader/upload codegen in dependencies, orthogonal to the rive↔wgpu sync interop this gate verifies.

### Residual risk (honest)

- **Native interlock-path: now CLOSED.** The Windows Vulkan SDK was installed and the matrix re-ran on
  the native NVIDIA ICD across both the Atomics and the Clockwise/interlock PLS paths — all interop-clean
  (above). The earlier deferral is resolved.
- **Raster-ordering PLS mode** (`VK_EXT_rasterization_order_attachment_access`) is **not exercised** —
  the RTX 4090 doesn't advertise that extension, so rive never selects that mode here. It records a third
  barrier variant; attesting it needs hardware that advertises the extension (a follow-up, not blocking).
- The upstream `VUID-StandaloneSpirv-None-10684` (naga shader codegen) and the cosmetic first-frame
  (review finding #2) are both non-interop and documented above; neither is a sync-correctness risk.

---

## Task 2 — full multi-agent adversarial review of the sync interop

Five independent finder agents, one per risk dimension (timeline-semaphore, `as_hal` recording,
layout/ordering, recycling/pacing, M1b invariants). The first run reproduced the M2a failure mode — all
five agents analyzed but never emitted via the forced `StructuredOutput` tool (lost, ~112 min); the
re-run used **free-text JSON output** (no tool requirement) and tighter scope, yielding 4 findings. Each
was adversarially verified against the vendored source before any fix.

| # | sev | finding | verdict |
|---|---|---|---|
| 1 | high | Timeline gate checks **physical-device** support, not the feature **enabled** on wgpu's logical device → if a GPU supported timeline but wgpu didn't enable it, `create_semaphore(TIMELINE)` is UB / stuck-or-wrong watermark → corruption | **Refuted (for pinned wgpu).** wgpu-hal 27 (`adapter.rs:343-348`) enables `timelineSemaphore` **iff** the physical device supports it (core ≥1.2, else it pushes+enables `VK_KHR_timeline_semaphore`). So the physical `features2` check is *equivalent* to logical-device enablement at wgpu `=27.0.4`; the corruption path cannot occur. Coupling documented in code; revisit on a wgpu bump. |
| 2 | low | First rendered frame can be **wiped** (wgpu lazy-zero-inits the shared texture — written only by rive's foreign commands — before the display pass samples it) | **Confirmed, cosmetic.** Frame 1 is empirically black even though the node logs the un-premult pass; it stabilizes by ~frame 2 (could be wgpu lazy-init and/or the display-image→sprite pipeline lag). **Steady-state is verified unaffected** (the byte-identical md5 + validation runs are post-warmup). Documented; fix deferred (it would add a clear pass / behavior change — out of scope for a verification milestone). |
| 3 | high | The `add_signal_semaphore` signal rides the **next** queue submit; a screenshot/out-of-graph submit could drain it and mis-pair the watermark → recycle a buffer rive still reads → corruption | **Refuted.** Bevy records screenshot copies via `submit_screenshot_commands(world, encoder)` as the graph **finalizer** (`renderer/mod.rs:55`) into the *same* `render_context` encoder that the **single** per-frame `queue.submit` flushes (`graph_runner.rs:80-87`). There is no separate screenshot submit; asset/staging submits run in `Prepare`, before our node arms the signal. The signal always rides rive's own frame submit. |
| 4 | low | The fixed-ring fallback's present-mode safety **warning latches** after the first frame (gated on the one-shot PLS-log flag) → a runtime vsync→Immediate change into the unsafe regime never re-warns | **Fixed.** The guard now evaluates **every frame** with its own flag, warning once per *entry* into the unsafe regime (resets when safe) so a runtime present-mode transition re-warns. Fallback-path only; the watermark path has no precondition. |

The timeline finder also raised (in its reasoning, not as a filed finding) whether `RiveFrameSync::drop`
could race a final wgpu submit draining a pending `(sema, frame_no)` signal against the just-destroyed
semaphore. **Handled:** the Drop does `device_wait_idle` *before* `destroy_semaphore`, so no pending
submit references it; the post-fix validation runs show zero teardown VUIDs.

**Net:** 2 high findings refuted with source evidence, 1 low fixed (warn-guard), 1 low documented
(cosmetic first frame). Fixes re-verified: Dozen + native matrices VUID- and SYNC-HAZARD-clean, native
output **byte-identical** (`md5 58c894e0…`). Combined with Task 1's validation gate (which caught + fixed
the real semaphore-leak VUID), the M2a+M2b sync interop is now independently attested correct on **Dozen
and native NVIDIA, across both the Atomics and Clockwise/interlock PLS paths**. _Same-author note: the
verification of each finding was done by me reading the vendored source; the finding generation was
multi-agent._

---

## Guardrails honored

No new features (no clockwise / in-place upload / pipelining). Every attestation traces to the per-cell
table in [docs/perf/m2v_attestation.txt](perf/m2v_attestation.txt), with one representative raw log per
environment kept under `docs/perf/m2v_logs/` (the per-cell logs were near-identical). The one VUID
surfaced in our code was fixed, not rationalized.
