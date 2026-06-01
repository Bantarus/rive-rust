# Milestone 3.0 — D3D12 fast-path feasibility spike (source-read + floor check; no bridge code)

The M1b/M2 zero-copy tier is Vulkan-specific: a shared `VkDevice`/`VkImage`, rive's
`flush()` recording into wgpu's own `VkCommandBuffer`, and a timeline-semaphore
completion watermark. Native Windows defaults to **D3D12**, where none of those
exist. Bringing the fast path to default-Windows therefore means rive's **D3D12
backend** driving wgpu's `ID3D12Device`. Before building that bridge (M3a), this
milestone **spikes its feasibility** — it confirms each load-bearing primitive has a
D3D12 equivalent by reading the real sources, and confirms the CPU-copy floor already
runs on D3D12. **Verdicts only; no bridge code, no shim D3D entry points.**

**Bottom line:** the highest-risk primitive (rive recording into a caller-provided
command list against a caller-provided device) is **GO** — rive's D3D12 backend is a
near-exact structural mirror of its Vulkan backend. No primitive is BLOCKED. There is
**one real adaptation** (and a smaller second one), neither of which the milestone
prompt predicted in the right place:
- **wgpu-hal's dx12 backend does not expose the open command list** (its Vulkan backend
  does, via `raw_handle()`), so the M2a "record-into-wgpu's-own-buffer" trick needs a
  tiny accessor or a one-extra-submit fallback.
- rive's D3D12 backend leaves the target texture in `COMMON` (not a shader-read state),
  so the resource-state handoff to wgpu's tracker must be reconciled.

Verdict → per the prompt's decision tree, this is the **"one ADAPT → greenlight M3a,
build with the adaptation noted"** outcome.

---

## Step 0 — the CPU-copy floor on default-D3D12 (empirical) — **YES**

Ran the M1a floor example (`sprite_riv`, the CPU-copy bridge) on the relay with wgpu
let to pick its Windows default. `win.cmd` hardcodes `WGPU_BACKEND=vulkan`, but that
`set` is `setlocal`-scoped and discarded when the script returns, so the run (outside
the script, with `WGPU_BACKEND=dx12`) gets the real D3D12 default.

Decisive evidence — wgpu selected D3D12, and rive rendered:
```
INFO bevy_render::renderer: AdapterInfo { name: "NVIDIA GeForce RTX 4090", …,
                                          device_type: DiscreteGpu, backend: Dx12 }
INFO sprite_riv: rive: wrote cap_m3_floor_d3d12.png.offscreen.png
INFO bevy_render::view::window::screenshot: Screenshot saved to cap_m3_floor_d3d12.png
INFO sprite_riv: rive: capture complete, exiting
```
The octopus renders correctly (upright, reference colors) in both the raw offscreen
RGBA and the composited window. `rc=0`. **So the CPU-copy floor is a working
Windows-D3D12 path today** — wgpu/Bevy on D3D12, rive offscreen, CPU copy between.

**Vulkan-ICD caveat:** the build still compiles and links `rive_vk_bootstrap`
(`vulkan_library.cpp`'s `LoadLibraryA("vulkan-1.dll")`, etc.) — rive renders its frame
on its **own self-managed Vulkan device**, and the bridge CPU-copies the result into
a Bevy `Image`. So the floor on D3D12 **still requires a Vulkan ICD present**. A truly
Vulkan-free Windows build is *not* what the floor delivers; it needs rive's D3D12
backend — exactly the M3a fast path. (Evidence: `docs/perf/m3_0_d3d12_floor.txt`.)

---

## Step 1 — the four load-bearing primitives (source-read verdicts)

| # | Primitive | Verdict | Decisive source |
|--:|---|:--:|---|
| 1 | rive D3D12 external context + external-record flush | **GO** | `render_context_d3d12_impl.cpp:557` (caller `ID3D12Device`), `:1041-1056` (records into caller `CommandLists`, never submits) |
| 2 | wgpu-hal dx12 exposes the frame's **open** `ID3D12GraphicsCommandList` | **ADAPT** | `wgpu-hal .../dx12/mod.rs:861` (`list` private, **no accessor**) vs `vulkan/mod.rs:1250` (`raw_handle()`) |
| 3 | D3D12 completion watermark (timeline fence) | **GO** | `dx12/mod.rs:739-741` (`Queue::as_raw`), post-submit `Signal` (`mod.rs:1559`) + `GetCompletedValue` (`device.rs:2255`) |
| 4 | handle extraction + `ID3D12Resource` wrap + state handoff | **GO** (extract+wrap) / **ADAPT** (state) | `dx12/mod.rs:935-939` (`raw_resource`); rive wrap `…d3d12_impl.cpp:409`; rive leaves `COMMON` `:2027-2030` |

### 1. rive D3D12 external-record mode — **GO** (the highest-risk question, verified twice)

rive's classic D3D12 backend (`renderer/src/d3d12/`) is a structural mirror of its
Vulkan backend: **the caller owns the device, the command lists, the queue, and the
submission; rive only records.**

- **External device.** `RenderContextD3D12Impl::MakeContext(ComPtr<ID3D12Device> device,
  ID3D12GraphicsCommandList* copyCommandList, const D3DContextOptions&)`
  ([render_context_d3d12_impl.cpp:557](vendor/rive-runtime/renderer/src/d3d12/render_context_d3d12_impl.cpp#L557))
  takes a *borrowed* device (stored as `m_device`, never created internally) and creates
  **no command queue** of its own. The fiddle harness proves the model: it calls
  `D3D12CreateDevice` itself and hands the device to `MakeContext`.
- **External-record flush** *(the make-or-break)*. `flush()` pulls a caller-provided
  `CommandLists{copyComandList, directComandList}` out of `FlushDescriptor::externalCommandBuffer`
  and records every draw/dispatch/barrier into it, then **returns without submitting** —
  no `ExecuteCommandLists`, `Signal`, or fence-wait. Verified directly:
  [render_context_d3d12_impl.cpp:1041-1056](vendor/rive-runtime/renderer/src/d3d12/render_context_d3d12_impl.cpp#L1041).
  The caller (= wgpu) closes and submits. The only internal submit in the whole impl is
  the optional **canvas** path (`commitCommandBuffer`, gated on `m_canvasQueue`, null
  unless the caller opts in) — irrelevant to the bridge.

This is the same external-device + external-record contract the Vulkan bridge already
relies on (Vulkan `flush` records into `reinterpret_cast<VkCommandBuffer>(desc.externalCommandBuffer)`).
**The non-blocking design is sound on rive's side.** The only nuance is on wgpu's side
(primitive #2) — getting wgpu's open list to hand to rive.

### 2. wgpu-hal dx12 open command list — **ADAPT** (the real adaptation this spike found)

The M2a non-blocking design hands rive *wgpu's own open command buffer* so rive's draws
ride along in wgpu's single submit. On Vulkan that works because the wgpu-hal **vulkan**
`CommandEncoder` exposes `pub unsafe fn raw_handle(&self) -> vk::CommandBuffer`
([vulkan/mod.rs:1250](crates/bevy-rive/src/zero_copy.rs#L1247) — used at `zero_copy.rs:1247-1254`).

The wgpu-hal **dx12** `CommandEncoder` holds the open list in
`list: Option<ID3D12GraphicsCommandList>` but the field is **private and has no
accessor** (`wgpu-hal-27.0.4/src/dx12/mod.rs:861`; the struct's only `impl` in that
region is `Debug`). So `CommandEncoder::as_hal_mut::<dx12::Api>()` yields a
`&mut dx12::CommandEncoder` you cannot read the list out of. The primitive as posed is
**not available in wgpu-hal 27.0.4**. Two clean adaptations, neither blocking:

- **(a) preferred — record into wgpu's list (exact M2a port).** Add a ~3-line
  `pub unsafe fn raw_list(&self) -> &ID3D12GraphicsCommandList` to the dx12
  `CommandEncoder`, mirroring vulkan's `raw_handle()`. Vendor-patchable today and a
  trivially upstreamable PR. Then hand that list to rive as the `directComandList`; rive
  records (it does not close — matching the `as_hal_mut` "don't end the buffer" contract,
  confirmed by flush returning after recording), wgpu closes + submits. 1:1 with M2a.
- **(b) fallback — rive owns its list + one extra same-queue submit.** rive records into
  its **own** `ID3D12GraphicsCommandList`; the bridge then does a separate
  `ExecuteCommandLists` on the shared queue via the exposed `Queue::as_raw()`. A single
  D3D12 queue executes in submission order, so this stays correct and **still
  non-blocking** (recycle via the watermark, primitive #3). Costs one extra submit per
  frame vs. (a), no wgpu-hal patch.

### 3. D3D12 completion watermark — **GO** (and simpler than Vulkan)

D3D12 fences are already monotonic timeline counters, and the exposure is sufficient.
`Queue::as_hal::<dx12::Api>()` → `dx12::Queue::as_raw() -> &ID3D12CommandQueue`
(`dx12/mod.rs:739-741`); with our own `ID3D12Fence` we call `queue.as_raw().Signal(fence,
frame_no)` **after** wgpu's submit (wgpu-hal itself does exactly `queue.Signal(fence,
value)` at `mod.rs:1559`), and read completion lock-free via
`ID3D12Fence::GetCompletedValue()` (used at `device.rs:2255`). `create_fence` even uses
`D3D12_FENCE_FLAG_SHARED` (`device.rs:2240`), and `Fence::raw_fence()` is public
(`mod.rs:1019-1032`). This is the precise structural analog of the M2b own-timeline-
semaphore watermark — and *simpler*, since there's no `add_signal_semaphore` plumbing,
just a post-submit `Signal`. (wgpu's own `SubmissionIndex` + `Device::poll` is a portable
fallback, but `poll` doesn't hand back the raw counter, so prefer the post-submit Signal
for a lock-free per-frame watermark.)

> Note: the prompt anticipated the *watermark* might be the "one ADAPT (post-submit
> Signal)". It is not — the watermark is GO. The adaptation landed at primitive #2.

### 4. handle extraction + resource wrap + state handoff — **GO** (extract+wrap), **ADAPT** (state)

- **Extraction — GO, all `pub`.** `Device::raw_device() -> &ID3D12Device`
  (`device.rs:430-432`), `Queue::as_raw() -> &ID3D12CommandQueue` (`mod.rs:739-741`),
  `Texture::raw_resource() -> &ID3D12Resource` (`mod.rs:935-939`, `unsafe fn`). Reached
  through Bevy's backend-agnostic `RenderDevice::wgpu_device()` /
  `RenderQueue`(`render_device.rs:259-260`) — the same access path the Vulkan bridge uses.
- **Resource wrap — GO.** `RenderTargetD3D12::setTargetTexture(ComPtr<ID3D12Resource>)`
  ([…d3d12_impl.cpp:409](vendor/rive-runtime/renderer/src/d3d12/render_context_d3d12_impl.cpp#L409))
  adopts an existing resource (`makeExternalTexture`, `:432`). Requirements: format
  `R8G8B8A8/B8G8R8A8_UNORM` (or `_TYPELESS`) and matching size; rive builds its own
  RTV/SRV/UAV heaps. If the wgpu texture lacks `ALLOW_UNORDERED_ACCESS`, rive
  transparently falls back to an internal UAV target + blit.
- **State handoff — ADAPT.** rive issues its own `ResourceBarrier`s and leaves the target
  in `D3D12_RESOURCE_STATE_COMMON` at end of flush (`:2027-2030`, `:2011-2013`) — *not* a
  Vulkan-style shader-read layout — so wgpu's dx12 state tracker must be told the texture
  is in `COMMON` after rive's submit (COMMON's implicit promotion/decay is friendly for
  this). Second wrinkle: `setTargetTexture` seeds the tracked state to `PRESENT` (`:433`),
  so rive's first barrier is `PRESENT → RENDER_TARGET`; for a wgpu-owned (non-swapchain)
  texture the real prior state differs and must be reconciled (hand it to rive in/decayed
  to `COMMON`, or extend the wrap API to set the initial tracked state). Mechanism is
  fully present; the exact state values are the porting work.

---

## Overall recommendation — greenlight M3a, with two adaptations noted

The D3D12 fast-path bridge is a **clean port of the Vulkan design**, not a rewrite. The
load-bearing pair (external device + external-record flush) is GO on rive's side; the
watermark and all raw-handle extraction are GO on wgpu's side. Build M3a, accounting for:

1. **Open-list exposure (primitive #2)** — prefer adding a tiny `raw_list()` accessor to
   wgpu-hal's dx12 `CommandEncoder` (mirror vulkan's `raw_handle()`; vendor-patch +
   upstream PR) for a 1:1 M2a port; else use rive-owns-its-list + one extra same-queue
   `ExecuteCommandLists` via `Queue::as_raw()`.
2. **Resource-state handoff (primitive #4)** — reconcile rive's `COMMON`/`PRESENT`
   assumptions with wgpu's dx12 resource tracker.

Two build-integration items for M3a (not feasibility risks):
- Our shim currently builds/links only rive's **Vulkan** renderer (`--with_vulkan`,
  `build.rs` VULKAN_SOURCES). rive's D3D12 backend compiles by default on Windows
  (`premake5_pls_renderer.lua:463-468`, links `d3d12/dxgi/dxguid/d3dcompiler`, uses the
  standard `<d3dx12.h>`, no PIX/Agility dependency) — M3a must include those objects and
  add a `rive_render_context_create_d3d12_external` + `…wrap_d3d12_resource` shim entry
  point (the D3D12 analog of the existing Vulkan-external shim).
- The bridge crate must add `windows = "0.58"` (Direct3D12/Dxgi features) matching
  wgpu-hal's pin to name `ID3D12GraphicsCommandList`/`…CommandQueue`/`…Resource`/`…Fence`
  — the dx12 analog of the Vulkan path's `ash` version pin.

Alternative shipping options (if M3a is deferred): ship Windows on the **floor**
(works today, D3D12 + Vulkan-ICD, CPU copy), or on **forced-Vulkan** (`WGPU_BACKEND=vulkan`,
the M1b/M2 fast path — a legitimate config on the 4090 / Vulkan-capable Windows GPUs).

## Guardrails honoured
- **No bridge code, no shim D3D entry points, no new features.** Source-reading + the
  Step-0 floor smoke check only; every verdict traces to a cited source location.
- The Vulkan tier, the frozen ECS API, and the CPU-copy floor are untouched.

## Follow-ups (out of scope here)
- The `raw_list()` wgpu-hal dx12 accessor (upstream PR candidate) — unblocks the 1:1 M2a port.
- M3a proper: build the D3D12 external-context + external-record bridge with the two
  adaptations above; then Metal/D3D11 behind the same C ABI per the M3 plan.
