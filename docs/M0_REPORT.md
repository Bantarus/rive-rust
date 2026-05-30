# Milestone 0 — report

**Status: complete and verified.** A standalone Rust binary loads a `.riv`,
advances its default state machine by one frame, renders it with the **native
Rive Renderer** (rive-runtime's PLS renderer, Vulkan) into an offscreen image
managed by the shim's *own* `VkInstance`/`VkDevice`, reads the pixels back, and
writes a PNG. No wgpu, no Bevy, no shared-device code exists yet.

```
cargo run --example offscreen_png -- assets/coffee_loader.riv out.png
```

---

## 1. What built and what runs

- **rive-runtime** PLS Vulkan static libs, built from the pinned submodule
  (`vendor/rive-runtime` @ `3f868558`) via `premake5 → gmake2 → make`, fully
  driven from `crates/rive-renderer-sys/build.rs`. All ten archives produced:
  `librive.a` (150 MB debug), `librive_pls_renderer.a`, `librive_decoders.a`,
  `liblibpng.a`, `libzlib.a`, `liblibjpeg.a`, `liblibwebp.a`, `librive_harfbuzz.a`,
  `librive_sheenbidi.a`, `librive_yoga.a`.
- **C++ shim** (`shim/rive_shim.cpp`) + rive's `rive_vk_bootstrap` sources,
  compiled with clang and linked against the rive libs + the Vulkan loader.
- **`rive-renderer-sys`** raw FFI and **`rive-renderer`** safe RAII wrapper
  compile clean (`cargo clippy` clean, `cargo fmt --check` clean, 2 unit tests
  pass).
- **`examples/offscreen_png.rs`** runs end-to-end (exit 0) on two samples:
  - `coffee_loader.riv` → `out.png` (512×512 RGBA, 30,104 / 262,144 non-background pixels).
  - `octopus_loop.riv` → `out_octopus.png` (512×512 RGBA, richer content).

### The output PNG

A correctly-shaped, **upright**, **centered** coffee-mug outline (the
`coffee_loader` artboard's first frame) on the opaque dark-gray clear color —
correct orientation (no vertical flip), correct fit (`Fit::contain` +
`Alignment::center`), correct colors (no double gamma, no straight-alpha
artifacts). The render is faint because `coffee_loader` at ~16 ms shows only the
empty mug before its fill animates — accurate for "one frame". `octopus_loop`
renders a full, vivid illustration.

### Device actually used (the important hardware finding)

```
Vulkan 1.2.335 GPU (Discrete): Microsoft Direct3D12 (NVIDIA GeForce RTX 4090)
driver 26.0.0  [ independentBlend, fillModeNonSolid, fragmentStoresAndAtomics, shaderClipDistance ]
```

Under WSL2 **there is no native NVIDIA Vulkan ICD** — only Mesa's **`Dozen`**
(Vulkan-on-D3D12, wrapping the 4090) and `llvmpipe` (CPU). `Dozen` exposes
**neither** `VK_EXT_fragment_shader_interlock` **nor**
`VK_EXT_rasterization_order_attachment_access`, so the Rive Renderer runs its
**atomic** PLS path (it has `fragmentStoresAndAtomics`). The output is correct;
this is just the slower path. **This directly affects M1** (see §6).

---

## 2. Exact toolchain used

| Tool | Version |
| --- | --- |
| rustc / cargo | 1.94.1 |
| clang / clang++ | 18.1.3 (Ubuntu) |
| premake5 | 5.0.0-beta2 (vendored at `tools/premake5`) |
| GNU make | 4.3 |
| python3 | 3.12.3 |
| glslangValidator | 15.1.0 |
| spirv-opt | SPIRV-Tools v2025.1 |
| git | 2.43.0 |
| Vulkan-Headers (vendored by premake) | `vulkan-sdk-1.4.321` |
| VMA (vendored by premake) | `v3.3.0` |
| OS | Ubuntu 24.04 on WSL2, kernel 6.6 |

rive libs are built **debug** (`--config=debug`) to avoid release LTO's
LLVM-bitcode archives, which can confuse a non-LLVM final linker. Override with
`RIVE_RUNTIME_CONFIG=release`.

---

## 3. How the shim maps onto the real rive-runtime API

The original ABI sketch was from memory of the RiveSharp pattern; the real API
differs. The shim follows the **real source** (verified against the headers and
`tests/common/testing_window_vulkan_texture.cpp`, rive's own offscreen path):

| Shim entry point | Real rive-runtime calls |
| --- | --- |
| `rive_render_context_create_vulkan_self` | `rive_vkb::VulkanInstance::Create` → `VulkanDevice::Create({headless=true})` → `RenderContextVulkanImpl::MakeContext(instance, physDev, dev, device.vulkanFeatures(), getVkGetInstanceProcAddrPtr(), {forceAtomicMode})` |
| `rive_render_target_create_offscreen` | `VulkanHeadlessFrameSynchronizer::Create(...)` (offscreen `VkImage` + readback) + `impl->makeRenderTarget(w, h, R8G8B8A8_UNORM, COLOR_ATTACHMENT\|TRANSFER_SRC\|TRANSFER_DST)` |
| `rive_file_load` | `rive::File::import(Span, /*Factory=*/renderContext, &result)` — `RenderContext` **is-a** `Factory` |
| `rive_file_artboard_default` | `File::artboardDefault()` → `unique_ptr<ArtboardInstance>` |
| `rive_artboard_state_machine_default` | `ArtboardInstance::defaultStateMachine()`, falling back to `defaultScene()`; stored as `unique_ptr<Scene>` |
| `rive_state_machine_advance` | `Scene::advanceAndApply(dt)` |
| `rive_frame_begin` | `sync->beginFrame()` + `rt->setTargetImageView(sync image/view/access)` + `renderContext->beginFrame(FrameDescriptor{w, h, clear, colorARGB(...)})` + `new RiveRenderer(ctx)` |
| `rive_artboard_draw` | `computeAlignment(Fit::contain, Alignment::center, frame, artboard->bounds())` → `renderer->save/transform/draw/restore` |
| `rive_frame_flush` | `renderContext->flush(FlushResources{rt, externalCommandBuffer=sync->currentCommandBuffer(), currentFrameNumber, safeFrameNumber})` + `sync->queueImageCopy` + `sync->endFrame` + `sync->getPixelsFromLastImageCopy` |
| `rive_render_target_read_pixels` | copies the captured `vector<uint8_t>` (premultiplied, top-down RGBA8; the shim flips `getPixelsFromLastImageCopy`'s bottom-up output back to top-down) |

### C ABI deviations from the sketch (flagged per the prompt)

1. **`rive_vk_bootstrap` is compiled into the shim.** rive does not build it into
   any static lib; its headless instance/device/synchronizer are exactly the M0
   self-managed-Vulkan + offscreen + readback path, so the shim reuses it rather
   than re-implementing instance/device/VMA/command-buffer/fence bring-up.
2. **`RenderTarget` bundles the synchronizer.** Pixel readback lives on
   `VulkanFrameSynchronizer`, not the render target — but it is exposed through
   the `RiveRenderTarget` handle (`rive_render_target_read_pixels`).
3. **No separate `Factory`.** The `RenderContext` is passed directly as the
   `rive::Factory` to `File::import`.
4. **"State machine" is a `Scene`.** `defaultStateMachine()` returns null unless
   the designer flagged one, so the shim falls back to `defaultScene()`.
5. **`forceAtomicMode`** is exposed via the `RIVE_FORCE_ATOMIC` env var, and GPU
   selection via `RIVE_GPU` (honored by `VulkanDevice`).
6. **Added `_destroy` entry points** for artboard and state machine (RAII
   completeness; the sketch omitted them).
7. **Readback is premultiplied, top-down RGBA8.** The wrapper provides
   `unpremultiply_rgba8`; the example uses an opaque clear (premultiplied ==
   straight) for a guaranteed-correct first image.

---

## 4. Gotchas encountered (especially premake-on-Linux)

1. **premake working directory.** premake must run from `vendor/rive-runtime/renderer/`
   — it anchors `RIVE_BUILD_OUT` and the generated-shader include path to its CWD,
   and `--out` is string-concatenated onto it (so `--out` must be **relative**).
2. **`make` config name is `default`.** rive declares a single premake
   configuration literally named `default`; debug/release is baked at premake
   time (`--config=…`). `make config=release` errors.
3. **Never `make all`.** The default target builds the `path_fiddle` demo, which
   needs GLFW. Pass explicit static-lib targets.
4. **Double-`lib` archive names.** premake prefixes `lib`, so `libpng`/`libjpeg`/
   `libwebp` projects produce `liblibpng.a` etc. (link names stay `libpng` …).
5. **rive_vk_bootstrap headers have no include guards.** Including both
   `vulkan_frame_synchronizer.hpp` and `vulkan_headless_frame_synchronizer.hpp`
   (the latter includes the former) caused a `redefinition` — include only the
   headless header.
6. **Bootstrap sources need `renderer/src` on the include path** to resolve
   `#include "shaders/constants.glsl"` (a GLSL/C++ shared constants file), plus
   the generated-headers dir.
7. **Vulkan header precedence.** The pinned `vulkan-sdk-1.4.321` headers must be
   listed **first** in the shim's `-I` order, or the system `/usr/include/vulkan`
   header wins and a rive source fails on `VK_KHR_SWAPCHAIN_EXTENSION_NAME`
   (version skew). This also keeps the shim ABI-consistent with the rive libs.
8. **WSL2 / Dozen / atomic path** — see §1 and §6.
9. **Validation layers absent.** Debug builds default to requesting
   `VK_LAYER_KHRONOS_validation`; with no `vulkan-validationlayers` package this
   prints a warning. The shim disables validation (`VulkanValidationType::none`).
   The `dzn is not a conformant Vulkan implementation` line comes from the Mesa
   driver itself and is expected on WSL2.
10. **Color contract.** Target is `VK_FORMAT_R8G8B8A8_UNORM` (non-sRGB ⇒ no
    hardware gamma; bytes are sRGB-encoded as a PNG wants). Renderer output is
    premultiplied.
11. **Vertical flip (found via review).** rive's Vulkan backend renders top-down,
    but `getPixelsFromLastImageCopy` flips rows to a GL-style bottom-up
    convention — rive's own test PNG writer flips a *second* time to compensate.
    Doing only the one flip renders the image **upside down** (obvious on the
    octopus sample, hidden on the near-symmetric coffee mug). The shim flips back
    so `read_pixels` returns genuine top-down RGBA8.

---

## 5. Definition of done — checklist

- [x] `cargo run --example offscreen_png -- assets/sample.riv out.png` produces a
      visually correct PNG of the artboard's first frame.
- [x] Reproducible from a clean checkout given the documented prerequisites
      (`git submodule update --init`, `apt-get install …`, vendored premake5).
- [x] No wgpu, no Bevy, no external-device/shared-image code.
- [x] `BUILD.md` records exact toolchain versions and gotchas.
- [x] RAII handles free on `Drop`; `Result`-based errors; no panics across FFI.

---

## 6. Proposed C ABI additions for M1 (for review — not implemented)

M1 hands wgpu's device to Rive and renders into a wgpu-allocated `VkImage`.
Concretely:

```c
// wgpu owns the device; Rive borrows its handles. Mirrors MakeContext, but with
// caller-supplied handles instead of rive_vk_bootstrap creating them. The caller
// must also pass the VulkanFeatures it enabled (so Rive picks the right PLS path).
RiveRenderContext* rive_render_context_create_vulkan_external(
    VkInstance, VkPhysicalDevice, VkDevice, VkQueue, uint32_t queueFamilyIndex,
    /* + a VulkanFeatures mirror struct + PFN_vkGetInstanceProcAddr */);

// Wrap a wgpu-allocated VkImage (pulled via Texture::as_hal) as a Rive target.
// Maps to impl->makeRenderTarget(w, h, fmt, usage) + RenderTargetVulkanImpl::
// setTargetImageView(view, image, lastAccess). For zero-copy/PLS-direct the image
// SHOULD be created with COLOR_ATTACHMENT | INPUT_ATTACHMENT usage; otherwise Rive
// round-trips through an internal offscreen texture.
RiveRenderTarget* rive_render_target_wrap_vk_image(
    RiveRenderContext*, VkImage, uint32_t w, uint32_t h, VkFormat);
```

Design notes that fall out of the M0 work:

- **Feature negotiation is the caller's job.** Rive enables *no* device features;
  `VulkanFeatures` is purely a report of what was enabled at device creation.
  M1 must enable the desired features in wgpu via
  `wgpu_hal::vulkan::Adapter::open_with_callback` (modify the `pNext`/extension
  lists before wgpu creates the `VkDevice`) and mirror them into `VulkanFeatures`.
  The M0 shim already marks where this struct is built.
- **`vkGetInstanceProcAddr`.** `MakeContext` needs a real loader that can resolve
  `vkGetDeviceProcAddr` (rive is built `VK_NO_PROTOTYPES`). wgpu exposes the
  entry points via `ash`; the shim must accept/loader-bridge one.
- **No `rive_vk_bootstrap` in M1.** The frame loop no longer uses the headless
  synchronizer: `rive_frame_flush` takes an **external `VkCommandBuffer`** and
  the two frame-number counters from the host (wgpu), and sync is
  `device.poll(Wait)`/fence first (M1), then `CommandEncoder::transition_resources()`
  + same-queue ordering (M2).
- **On this WSL2 box specifically**, interlock/raster-order are unavailable, so
  M1 will exercise the atomic path. The `fragment-shader-interlock` device-feature
  request in `open_with_callback` should be *attempted* but tolerate absence
  (fall back to atomic), and the M0 PNG is a trustworthy reference for diffing
  the M1 result (same atomic path, same color contract).

---

## 7. Repo / git state

`git init` was run, `vendor/rive-runtime` added as a submodule (depth-1, pinned
`3f868558`, recorded as a `160000` gitlink), and the M0 work committed as the
initial commit on `master` (the foundational commit of a fresh local repo).
`target/`, `out*.png`, `.rive-deps/`, the in-submodule `out/` build tree, and the
fetched `tools/premake5` binary (+ its bundled `.so` extras) are git-ignored —
premake5 is obtained via `tools/fetch_premake.sh`, not tracked.
