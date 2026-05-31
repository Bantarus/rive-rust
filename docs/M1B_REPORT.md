# Milestone 1b — report (zero-copy shared `VkImage` via a Bevy Render Graph node)

**Status: complete and verified rendering on the real RTX 4090** — both the
**native Windows relay** (NVIDIA Vulkan ICD, driver 591.86) and **WSL2-via-Dozen**
(Mesa Dozen → D3D12 → the same 4090). The native Rive Renderer renders the `.riv`
**directly into a wgpu-allocated `VkImage`** — no per-frame CPU readback — driven by
a Bevy **Render Graph node**, sharing **one** Vulkan device with wgpu. The octopus
renders upright with correct colors through the shared image.

This is the **zero-copy Vulkan tier**. The M1a CPU-copy floor (see
[docs/M1A_REPORT.md](M1A_REPORT.md)) remains the default, non-regressing fallback;
M1b is gated behind the `zero_copy` cargo feature. The frozen M1a ECS API is
**unchanged** — the user still displays `RiveTarget.image` on a `Sprite`.

```
# WSL2, real 4090 via Dozen (fast local loop; atomic-only — see §5):
VK_DRIVER_FILES=/usr/share/vulkan/icd.d/dzn_icd.json DZN_DEBUG=experimental \
WGPU_ALLOW_UNDERLYING_NONCOMPLIANT_ADAPTER=1 RIVE_FORCE_ATOMIC=1 \
RIVE_CAPTURE=zc.png cargo run -p bevy-rive --features zero_copy --example sprite_riv_zerocopy

# Native Windows 4090 (authoritative interlock-capable path), via the relay:
cmd.exe /c "cd /d E:\DEV\rive-rust && set WGPU_BACKEND=vulkan&& set RIVE_CAPTURE=cap_zc.png&& \
  scripts\win.cmd run --release -p bevy-rive --features zero_copy --example sprite_riv_zerocopy"
```

Pinned to **Bevy 0.18.1**, **wgpu/wgpu-hal `=27.0.1`/`=27.0.4`** (exact — the
`as_hal` hal types must match Bevy's), **ash 0.38**. The full design spec (with
rive-source- and registry-source-verified citations) is at
[docs/design/M1B_DESIGN_SPEC.md](design/M1B_DESIGN_SPEC.md).

---

## 1. What builds and what runs

| Platform | Adapter | Zero-copy fill | Composite | PLS mode |
| --- | --- | --- | --- | --- |
| **Windows** | **native RTX 4090, Vulkan** (driver 591.86) | ✓ shared `VkImage` | ✓ octopus, correct colors | **Atomics** (raster-order unsupported — see §5) |
| **Linux** (WSL2) | **real RTX 4090 via Dozen** → D3D12 | ✓ shared `VkImage` | ✓ octopus, correct colors | Atomics (Dozen is atomic-only) |

Both runs reach the render-graph node, lazily create rive's external context on
wgpu's device, render each frame into the shared texture out-of-band, run the
display pass, and exit cleanly (`capture complete, exiting`) with **no validation
errors and no frame failures**. The native release build is clean
(`Finished release profile`). Captures: `cap_zc_native2.png` (native),
`/tmp/zc_unpremult.png` (Dozen).

> **Why validate on native, not just Dozen:** Dozen is a non-conformant
> Vulkan→D3D12 translation layer (atomic-only, no interlock). It is a strong
> *functional* signal for the zero-copy memory-sharing path, but the
> **authoritative** interop validation is the native NVIDIA Vulkan ICD. An earlier
> blank composite turned out to be a real bug (below), reproduced identically on
> *both* — exactly what native validation is for.

---

## 2. Architecture (what M1b adds over M1a)

1. **Device sharing — Path A `raw_vulkan_init`.** `install_interlock_device_callback`
   inserts Bevy's `RawVulkanInitSettings` (before `DefaultPlugins`) whose callback
   runs *inside* Bevy's own `open_with_callback`: it appends the interlock device
   extension and chains the matching feature struct, so rive gets the device Bevy
   creates. Bevy keeps owning the wgpu device.
2. **Handle extraction.** A render-world `Prepare` system (`extract_shared_handles_once`)
   reads the raw `VkInstance/VkPhysicalDevice/VkDevice/VkQueue/queueFamily/loader`
   from Bevy's `RenderDevice`/`RenderAdapter`/`RenderInstance` via the guard-form
   `as_hal`, mirrors the actually-enabled features into a `rive_renderer::VulkanFeatures`,
   and stores them in a `RiveSharedHandles` resource.
3. **Shim external ABI.** `rive_render_context_create_vulkan_external` →
   `RenderContextVulkanImpl::MakeContext` (borrows the device, never destroys it).
   `rive_render_target_wrap_vk_image` wraps the wgpu texture's `VkImage` as a rive
   render target. rive's `flush()` **records into a caller-provided command buffer
   and never submits**, so the shim owns a per-frame command pool + one reused
   command buffer + an internal `VkFence`, records flush + the post-flush
   `COLOR→SHADER_READ_ONLY` barrier, submits **out-of-band** to wgpu's `VkQueue`,
   then blocks on the fence (M1b is correctness-first; non-blocking pipelining is M2).
4. **Render-graph node.** `RiveFillNode` (edged before `Node2d::StartMainPass` on
   `Core2d`) reads the extract resource, advances each state machine, renders into
   the **shared** texture, then runs the display pass into the user's `Image`.

rive's `!Send` objects live in a render-world resource `RiveRenderState(RefCell<RiveGpu>)`
with a hand-written `unsafe impl Send + Sync` justified by a strict single-render-thread
invariant (mirrors how Bevy makes wgpu objects `Send` via `WgpuWrapper`). The wrapper's
rive handles were refactored **`Rc → Arc`** so a cross-thread *drop* by Bevy's pipelined
renderer (the SubApp ferry) is sound (atomic refcount).

---

## 3. The display pass (un-premultiply + sRGB-decode)

rive renders **premultiplied, sRGB-encoded** bytes into the shared texture (linear
`Rgba8Unorm`, so the shader reads rive's raw bytes verbatim). A fullscreen-triangle
pass un-premultiplies in encoded space, sRGB-decodes, and writes the **linear
straight** result into the display `Image` (`Rgba8UnormSrgb`, whose store re-applies
the sRGB OETF). The `Sprite` then hardware-decodes on sample → straight-alpha OVER
matching M1a pixel-for-pixel, including partial alpha (design spec §7 Option B).

The blit pipeline samples with **`textureLoad`** (no sampler — it is a 1:1 same-size
copy indexed by integer destination pixel), so the WGSL matches the texture-only
bind-group layout. It is built **lazily in the node** (stored in `RiveGpu`), because
its `RenderDevice` only exists after `RenderPlugin::finish()` — not during plugin
`build()`.

---

## 4. The bug that made it blank (and the fix)

The first end-to-end runs composited **blank gray on both Dozen and native**. Root
cause: the per-entity work was keyed onto a `RenderEntity`, but the rive entities
were never synced to the render world (the code bypassed `ExtractComponentPlugin`,
which is what adds `SyncToRenderWorld`) → the node's query was empty → the node loop
never ran (PLS mode never advanced past its default; the copy diagnostic never
fired). **Fix:** extract into a render-world **resource** (`ExtractedRives: Vec`)
keyed by the main-world `Entity`, sidestepping entity-sync entirely. After the fix
the octopus renders on both backends.

---

## 5. PLS mode: the reality (corrects earlier drafts)

**On the native 4090, rive runs the `Atomics` PLS path** (`PLS mode = Atomics,
raster-order supported = false`), captured reliably this run. Earlier in-progress
notes variously claimed `RasterOrdering` or `Unknown` — **both were wrong**:

- `RasterOrdering` was an optimistic guess; it was never measured on good code.
- `Unknown` was a **diagnostic artifact**: rive's `frameInterlockMode()` is valid
  only *between* `beginFrame` and `flush`, but the node queried it *after* flush.
  Fixed by capturing the mode at `beginFrame` in the shim (`extLastInterlockMode`);
  the getter now returns that cached value. Dozen and native both report a clean
  mode after the fix.

**Why raster-ordering isn't selected — root cause (follow-up, does NOT block the
zero-copy DoD):** rive's `supportsRasterOrderingMode` depends on the
**`rasterizationOrderColorAttachmentAccess`** feature
(`VK_EXT_rasterization_order_attachment_access`), *not* on `fragmentShaderPixelInterlock`
(`render_context_vulkan_impl.cpp:960-962`). The device-create callback enables
`VK_EXT_fragment_shader_interlock`, which feeds rive's **clockwise** mode — a
*different* extension from the one raster-ordering needs. Since
`rasterization_order_attachment_access` is not enabled, raster-ordering is
unavailable and rive falls back to `Atomics`. (Note the lib log still says
"expecting raster-order PLS" off the `fragment_shader_interlock` heuristic — that
message is optimistic and should be reworded as part of this follow-up.)

Atomics renders correctly, so this is a **tier-quality** item. To get rive's
raster-ordering path on native: enable `VK_EXT_rasterization_order_attachment_access`
(+ its feature) in the device-create callback, and verify the chained feature struct
survives wgpu's `open_with_callback` `VkDeviceCreateInfo` rebuild.

---

## 6. Tiered bridge & frozen API (no regression)

- **M1a CPU-copy floor still works**, unchanged, as the default selectable fallback.
  Verified: the M1a example (`sprite_riv`, default features) and the whole workspace
  build green; M1b lives entirely behind the `zero_copy` feature.
- **The frozen M1a ECS API is unchanged**: `RiveFile`, `RiveAnimation`, `RiveTarget`,
  the selectors, and the `RiveTarget.image: Handle<Image>` upright seam. M1b swaps
  only the fill mechanism.

`fmt` and `clippy` are clean on our crates (bevy-rive, rive-renderer, rive-renderer-sys);
the only remaining rustfmt drift is pre-existing, committed, and outside M1b
(`build.rs`, the M0 `offscreen_png.rs` example).

---

## 7. Open items (before / into M2)

- **PLS tier quality** (§5): enable the correct raster-ordering extension/feature and
  confirm it survives `open_with_callback`; reword the optimistic interlock log.
- **Transparent compositing**: the un-premult pass is correct by construction for
  `a < 1`, but only opaque content (octopus) has been measured end-to-end; an
  explicit partial-alpha diff vs M1a is untested.
- **Native Linux Vulkan** (real driver, not Dozen) not yet run.
- **Image-layout ownership / `transition_resources`** (drop the blocking fence for
  pipelined non-blocking submit): M2.
- **No resize path** for the shared/display textures yet.
- **The single end-of-task adversarial review** is still owed (per house cadence:
  one review at the end, not per checkpoint).

---

## 8. How to verify

The `sprite_riv_zerocopy` example self-captures the composited window after a few
warm-up frames, then exits `AppExit::Success`. Drive it with the commands at the top
of this report. A successful run logs `AdapterInfo { … RTX 4090 … backend: Vulkan }`,
`rive zero-copy: PLS mode = Atomics, raster-order supported = false`,
`un-premult pass recorded shared->display … (GpuImage ready)`, and
`capture complete, exiting`; the captured PNG shows the upright octopus on the dark
clear color.
