# M1b Implementation Spec — Zero-Copy Vulkan Tier for rive-rust

**Status:** definitive, build-ready. Supersedes any earlier M1b sketch.
**Scope:** add a zero-copy Vulkan tier to the M1a CPU-copy bridge. M1a remains a selectable, non-regressing fallback. The M1a frozen ECS API (`RiveFile`+`RivLoader`, `RiveAnimation`/`RiveTarget`+`ArtboardSelector`/`StateMachineSelector`, the `Handle<Image>`+upright seam) is UNCHANGED — M1b swaps only the *fill mechanism*.

**Pins (from Cargo.lock):** bevy 0.18.1, wgpu 27.0.1, wgpu-core 27.0.3, wgpu-hal 27.0.4, naga 27.0.3, ash 0.38.0+1.3.281, wgpu-types =27.0.1. rive-runtime submodule runtime-v0.1.103 (`3f868558…`).

**Self-contained:** this spec is implementable from this file ALONE — no access to the `f*.md` research notes is required. Every API signature, call shape, struct, and constant needed is stated inline (rive C++ in §0; wgpu/wgpu-hal/Bevy in §0 + §5–§6). The `fN` / `[RES]` tags are *provenance breadcrumbs* (which research file a fact came from), never a "go read that file" dependency.

**Provenance note:** rive C++ signatures are verbatim from the vendored headers (cross-checked by direct reads this session; cited as f0–f4 = vendor/rive-runtime). Bevy/wgpu/wgpu-hal signatures are from the on-disk 27.0.1 / 27.0.4 / Bevy 0.18.1 sources (cited as f5–f9, f_extract). A few are tagged `[RES]` where they trace to the task's RESOLVED-FROM-RESEARCH summary rather than a line re-read this session; treat those as ground truth but re-verify the exact 27.x signature at implementation time (consolidated in §10 risk 5). File path citations like `render_context.hpp:234` / `adapter.rs:2358` are line anchors for the implementer's convenience, not prerequisites.

---

## 0. GROUND-TRUTH API INVENTORY (verbatim, do not re-derive)

### rive C++ (vendor)
```cpp
// render_context_vulkan_impl.hpp:40-52 — external-device context (NO VkQueue)
static std::unique_ptr<RenderContext> RenderContextVulkanImpl::MakeContext(
    VkInstance, VkPhysicalDevice, VkDevice,
    const VulkanFeatures&, PFN_vkGetInstanceProcAddr, const ContextOptions&);
// convenience overload defaults ContextOptions{}

// render_context_vulkan_impl.hpp:25-60 — ContextOptions
struct ContextOptions {
    bool forceAtomicMode = false;
    bool disableClockwiseFixedFunctionMode = false;
    ShaderCompilationMode shaderCompilationMode = ShaderCompilationMode::standard;
};

// render_context_vulkan_impl.hpp:64-77
VulkanContext* RenderContextVulkanImpl::vulkanContext() const;        // -> m_vk.get()
rcp<RenderTargetVulkanImpl> RenderContextVulkanImpl::makeRenderTarget(
    uint32_t width, uint32_t height,
    VkFormat framebufferFormat, VkImageUsageFlags targetUsageFlags);
void RenderContextVulkanImpl::setCanvasQueue(VkQueue, uint32_t);      // DO NOT USE (submits+waits)

// vulkan_context.hpp:17-40 — VulkanFeatures (11 fields, exact)
struct VulkanFeatures {
    uint32_t apiVersion = VK_API_VERSION_1_1;
    bool independentBlend = false;
    bool fillModeNonSolid = false;
    bool fragmentStoresAndAtomics = false;            // atomic-PLS fallback feature (MANDATORY for core)
    bool shaderClipDistance = false;
    bool rasterizationOrderColorAttachmentAccess = false; // EXT_rasterization_order_attachment_access
    bool fragmentShaderPixelInterlock = false;        // VK_EXT_fragment_shader_interlock (pixel only)
    bool VK_KHR_portability_subset = false;
    bool textureCompressionBC = false;
    bool textureCompressionASTC_LDR = false;
    bool textureCompressionETC2 = false;
};

// vulkan_context.hpp:151 — wrap a wgpu-owned image view WITHOUT allocating
rcp<vkutil::ImageView> makeExternalImageView(const VkImageViewCreateInfo&, const char* name);

// vulkan_context.hpp:194-199 — barrier helper the node records on the shared cmd buf
const vkutil::ImageAccess& simpleImageMemoryBarrier(
    VkCommandBuffer,
    const vkutil::ImageAccess& srcAccess,
    const vkutil::ImageAccess& dstAccess,
    VkImage,
    vkutil::ImageAccessAction = vkutil::ImageAccessAction::preserveContents,
    VkDependencyFlags = 0);

// vkutil.hpp:196 — ImageAccess (the seed/track struct; fields: pipelineStages, accessMask, layout)
struct vkutil::ImageAccess { /* VkPipelineStageFlags pipelineStages; VkAccessFlags accessMask; VkImageLayout layout; */ };

// render_target_vulkan.hpp:113-141 — the WRAP entry points (on the concrete Impl)
void RenderTargetVulkanImpl::setTargetImageView(VkImageView, VkImage, vkutil::ImageAccess targetLastAccess);
const vkutil::ImageAccess& RenderTargetVulkanImpl::targetLastAccess() const;
void RenderTargetVulkanImpl::updateLastAccess(const vkutil::ImageAccess&);    // after caller -> SHADER_READ_ONLY
VkImage RenderTargetVulkanImpl::targetImage() const;
VkImageView RenderTargetVulkanImpl::targetImageView() const;

// render_target_vulkan.cpp:21-28 — ctor debug-assert (usage contract)
//   assert((usage & INPUT_ATTACHMENT) || (usage & (TRANSFER_SRC|TRANSFER_DST)) == (TRANSFER_SRC|TRANSFER_DST));

// render_context.hpp:147,234-253 — PUBLIC frame model + caller-owned cmd buffer
void RenderContext::beginFrame(const FrameDescriptor&);
struct RenderContext::FlushResources {
    RenderTarget* renderTarget = nullptr;
    void* externalCommandBuffer = nullptr;     // VkCommandBuffer on Vulkan
    uint64_t currentFrameNumber = 0;           // frame being recorded
    uint64_t safeFrameNumber = 0;              // highest GPU-completed frame (from OUR fence)
};
void RenderContext::flush(const FlushResources&);  // RECORDS into externalCommandBuffer; NEVER submits
const gpu::PlatformFeatures& RenderContext::platformFeatures() const;   // frame-independent caps
const gpu::InterlockMode RenderContext::frameInterlockMode() const;     // valid only between begin/flush

// gpu.hpp:784-799 — InterlockMode (default int; 5 variants)
enum class InterlockMode { rasterOrdering=0, atomics=1, clockwise=2, clockwiseAtomic=3, msaa=4 };
// INTERLOCK_MODE_COUNT == 5

// gpu.hpp PlatformFeatures (relevant fields)
//   bool supportsRasterOrderingMode; bool supportsAtomicMode; supportsClockwise*; ...
// Vulkan translation (render_context_vulkan_impl.cpp:960-988):
//   supportsRasterOrderingMode = features.rasterizationOrderColorAttachmentAccess
//   supportsAtomicMode         = features.fragmentStoresAndAtomics
```

### Vulkan device parity rive needs `[from f3]`
- REQUIRED `VkPhysicalDeviceFeatures`: `fragmentStoresAndAtomics` (MANDATORY — the atomic fallback), `fillModeNonSolid`; plus `independentBlend`, `shaderClipDistance` if present.
- API version ≥ 1.1 (rive enforces in MakeContext; 1.3 ideal). wgpu default satisfies.
- INTERLOCK (optional, to get the clean `rasterOrdering` PLS path; else rive falls back to `atomics` on `fragmentStoresAndAtomics`):
  - Option A (most desktop GPUs incl. NVIDIA): device ext `VK_EXT_fragment_shader_interlock` + chain `VkPhysicalDeviceFragmentShaderInterlockFeaturesEXT{ fragmentShaderPixelInterlock = VK_TRUE }`.
  - Option B (AMD): device ext `VK_EXT_rasterization_order_attachment_access` (or AMD alias) + chain `VkPhysicalDeviceRasterizationOrderAttachmentAccessFeaturesEXT{ rasterizationOrderColorAttachmentAccess = VK_TRUE }`.
- The `VulkanFeatures` we hand MakeContext MUST mirror EXACTLY what wgpu enabled on the device. No external-memory extension is needed (the VkImage is wgpu-allocated and shared in-process via the same VkDevice — there is no cross-process/cross-device export).

### wgpu / wgpu-hal 27 + Bevy 0.18.1 (verbatim from f6 + f8 + f_extract; corrections folded in)

**TWO device-sharing paths; M1b uses Path A (Bevy-native), keeps Path B as the documented fallback the locked constraint names.**

- **Path A — `raw_vulkan_init` (PRIMARY, f6 verdict + f_extract §5):** bevy_render 0.18.1 ships feature `raw_vulkan_init = ["wgpu/vulkan"]` (bevy_render Cargo.toml:44). Insert a `RawVulkanInitSettings` resource BEFORE `DefaultPlugins`; call its `unsafe add_create_device_callback(|args: &mut wgpu::hal::vulkan::CreateDeviceCallbackArgs, adapter: &wgpu::hal::vulkan::Adapter, feats: &mut AdditionalVulkanFeatures| { … })` (raw_vulkan_init.rs:57-68). Bevy runs the callback INSIDE its own `Adapter::open_with_callback` (after wgpu computes `required_device_extensions` + `physical_device_features`, before `vkCreateDevice`), then `create_device_from_hal::<Vulkan>`. Bevy owns the wgpu device; we only inject the interlock extension. We then extract the raw handles from the resulting Bevy `RenderDevice`/`RenderAdapter`/`RenderInstance` via `as_hal`. This is strictly easier than Path B and the recommended approach.
- **Path B — `RenderCreation::Manual` (FALLBACK, the locked-constraint path):** build wgpu `Instance/Adapter/Device/Queue` ourselves via `hal_adapter.open_with_callback(...)` + `adapter.create_device_from_hal(...)`, then `RenderCreation::Manual(RenderResources(RenderDevice, RenderQueue, RenderAdapterInfo, RenderAdapter, RenderInstance [, AdditionalVulkanFeatures]))`. Use only if Path A proves insufficient or if rive must own the VkInstance/VkDevice. `WgpuSettings.features` CANNOT express the interlock Vk extensions (wgpu-types 27.0.1 has NO interlock Feature) — both paths drop to the Vulkan layer.

**`open_with_callback` (wgpu-hal 27.0.4 adapter.rs:2358) — the FnOnce callback signature is verbatim:**
```rust
pub unsafe fn open_with_callback<'a>(&self, features: wgt::Features, memory_hints: &wgt::MemoryHints,
    callback: Option<Box<super::CreateDeviceCallback<'a>>>) -> Result<crate::OpenDevice<super::Api>, DeviceError>
// CreateDeviceCallback<'this> = dyn for<'arg,'pnext> FnOnce(CreateDeviceCallbackArgs<'arg,'pnext,'this>) + 'this
// CreateDeviceCallbackArgs { extensions: &mut Vec<&CStr>, device_features: &mut PhysicalDeviceFeatures,
//                            queue_create_infos: &mut Vec<vk::DeviceQueueCreateInfo>, create_info: &mut vk::DeviceCreateInfo, _phantom }
// create_device_from_hal::<A>(open: OpenDevice<A>, desc: &DeviceDescriptor) -> Result<(Device, Queue), _>   (wgpu api/adapter.rs:76)
```
CRITICAL (f8/f6): set extensions via `args.extensions.push(c"VK_EXT_fragment_shader_interlock")` and the feature via `args.device_features` and/or by `push_next`-chaining the EXT struct onto `args.create_info` pNext — wgpu OVERWRITES `create_info`'s extension-name arrays / enabled_features / queue infos after the callback from the `extensions`/`device_features` fields. Verify the physical device advertises the extension first (else `vkCreateDevice` → `ERROR_EXTENSION_NOT_PRESENT` → hal panic). The chained EXT feature struct must outlive `vkCreateDevice` (ash `'pnext` lifetime hazard).

**`as_hal` is the GUARD form, NOT closures (f8 — corrects a common assumption):**
```rust
// wgpu 27.0.1: Texture/Device/Adapter/Buffer/Queue/TextureView use:  as_hal::<A>(&self) -> Option<impl Deref<Target=A::X>>
//              Instance uses:  as_hal::<A>(&self) -> Option<&A::Instance>
//              (only CommandEncoder::as_hal_mut takes a closure)
let dev_g = unsafe { device.as_hal::<wgpu_hal::vulkan::Api>() }.unwrap();   // hold the guard while using the handles
let vk_device:   ash::vk::Device         = dev_g.raw_device().handle();     // device.rs:977 -> &ash::Device
let vk_phys:     ash::vk::PhysicalDevice = dev_g.raw_physical_device();     // device.rs:981
let vk_queue:    ash::vk::Queue          = dev_g.raw_queue();               // device.rs:985
let qfi:         u32                     = dev_g.queue_family_index();      // device.rs:969 (==0 today)
let dev_exts:    &[&CStr]                = dev_g.enabled_device_extensions();// device.rs:989 (source of truth for VulkanFeatures)
let inst_shared = dev_g.shared_instance();                                  // device.rs:993
let vk_instance: ash::vk::Instance       = inst_shared.raw_instance().handle();  // instance.rs:220
let gipa = /* from */ inst_shared.entry().static_fn().get_instance_proc_addr;    // instance.rs:216 -> &ash::Entry (PFN source)
// VkImage from a GpuImage's texture (guard; texture.rs:61 -> mod.rs:964):
let tex_g = unsafe { gpu_image.texture.as_hal::<wgpu_hal::vulkan::Api>() }.unwrap();
let vk_image: ash::vk::Image = unsafe { tex_g.raw_handle() };               // mod.rs:964
let view_g = unsafe { gpu_image.texture_view.as_hal::<wgpu_hal::vulkan::Api>() }.unwrap();
let vk_view: ash::vk::ImageView = unsafe { view_g.raw_handle() };           // mod.rs:995 (or pass 0 and let shim make the view)
```
GUARD-LIFETIME RISK (f8): the Texture guard holds a read-lock blocking `destroy()` until dropped; re-derive `vk_image`/`vk_queue` each frame inside the node and drop the guards promptly. Raw handles are `Copy` but valid only while the owning wgpu object lives. `queue_family_index()==0` (wgpu hardcodes `family_index=0`); pass that to rive.

- Display: f9 — sprite samples by FORMAT only; `Rgba8Unorm` view = NO hw decode, `Rgba8UnormSrgb` = hw decode. `BlendState::ALPHA_BLENDING` = straight-alpha OVER. sRGB EOTF constants from bevy_color srgba.rs:215-224.

### Bevy render-world wiring (verbatim from f7 + f_extract)
- **Extract per-entity main→render:** `ExtractComponentPlugin<C>` (extract_component.rs:163) runs `extract_component(QueryItem<'_,'_,QueryData>) -> Option<Out>` in `ExtractSchedule`, keyed by `RenderEntity`; only `SyncToRenderWorld`-synced entities extract. `ExtractResource` is singleton-only (wrong for per-entity). Carry only `Send` data (Handle<Image>, size, dt, speed snapshot).
- **GpuImage lookup:** `world.resource::<RenderAssets<GpuImage>>().get(&handle) -> Option<&GpuImage>` (render_asset.rs:214). `GpuImage{ texture: Texture (Deref wgpu::Texture), texture_view, .. }` (gpu_image.rs:16). Lazily allocated in `GpuImage::prepare_asset` (gpu_image.rs:64-136) during `RenderSystems::PrepareAssets` when `image.data == None`, passing `texture_descriptor` (usage/format) straight to `create_texture`.
- **Render-graph node (f7):** `Node::run<'w>(&self, graph, render_context: &mut RenderContext<'w>, world: &'w World)`. Device: `render_context.render_device().wgpu_device()`. Queue: `world.resource::<RenderQueue>()` (Derefs to `&wgpu::Queue`). Register on the RenderApp: `app.add_render_graph_node::<RiveFillNode>(Core2d, RiveFillLabel).add_render_graph_edges(Core2d, (RiveFillLabel, Node2d::StartMainPass))` — ordering before `StartMainPass` precedes ALL sampling (Core2dPlugin chains StartMainPass→MainOpaquePass→MainTransparentPass; the sprite pass is `MainTransparentPass`). Bevy submits the whole graph's command buffers ONCE after the graph runs, so our out-of-band rive submit (direct on the wgpu Queue) reaches the GPU before Bevy's graph buffers — but still needs a fence/semaphore + layout barrier to SHADER_READ_ONLY. Do NOT use `add_command_buffer` (it only enqueues for the single end-of-graph submit; no fence).
- **render-world state residency (CORRECTION, f7/f_extract):** `PipelinedRenderingPlugin` (in DefaultPlugins) moves the render world to a spawned OS thread; render-world **`NonSend` is fragile/thread-affine there**. So rive's `!Send` state must be a *normal Send+Sync `Resource`* wrapping the FFI pointers in a newtype with hand-written `unsafe impl Send + Sync` + a single-thread invariant comment (mirroring `wgpu_wrapper.rs:15-18`), lazily created ON the render thread (in a `RenderSystems::Prepare` system or in `Node::update`). `Node::run` executes serialized on one thread/frame, so the single-thread invariant holds. (This supersedes the "render-world NonSend" phrasing — use the unsafe-Send wrapper resource.)

---

## 1. ARCHITECTURE OVERVIEW

### Two tiers, one frozen seam
The frozen ECS surface is unchanged. The user still writes `(RiveAnimation::new(handle), RiveTarget::new(w,h))` and displays `RiveTarget.image` (a `Handle<Image>`). What differs is *who fills that Image and where rive’s state lives*:

| | M1a (CPU-copy floor) | M1b (zero-copy Vulkan) |
|---|---|---|
| rive Context | self-managed VkDevice, `NonSend` on **main** world | wgpu-shared VkDevice, in **render** world |
| rive `!Send` per-entity state | main world `NonSend` | render world **Send+Sync wrapper resource** (NOT NonSend — see below) |
| Device sharing | n/a | Bevy owns the device; interlock ext injected via `raw_vulkan_init` callback (Path A) |
| Image backing | CPU `Vec<u8>`, `MAIN+RENDER` usages, re-uploaded via `Assets::get_mut` | GPU `Rgba8Unorm` texture, `RENDER_WORLD`-only, `data: None`, written in place |
| Fill path | advance→flush→readback→`unpremultiply`→copy bytes | extract→render-graph node: rive records into a VkCommandBuffer→out-of-band submit→fence→layout barrier |
| Alpha in shared/CPU buffer | straight (un-premultiplied on CPU) | premultiplied (rive’s native bytes; cannot un-premultiply zero-copy) |
| Straight-alpha display | Sprite on `Rgba8UnormSrgb` | un-premult+sRGB-decode pass → `Rgba8UnormSrgb` → **same** Sprite |

### Where rive’s `!Send` state lives in M1b — CORRECTED (f7/f_extract)
**A normal Send+Sync render-world `Resource` that wraps the FFI pointers in a newtype with hand-written `unsafe impl Send + Sync` + a single-thread invariant — NOT a `NonSend` resource.** Reason (f7 risk, f_extract §4): `PipelinedRenderingPlugin` (in DefaultPlugins) moves the render world onto a spawned OS thread, and render-world `NonSend` data is documented as thread-affine to its init thread and fragile there. So:
- The rive external `Context` + per-entity `Artboard`/`StateMachine`/wrapped `RenderTarget` + per-frame `VkFence`s live in `RiveRenderState` (a `#[derive(Resource)]` holding a newtype `struct RiveGpu(/* rive handles */); unsafe impl Send for RiveGpu {} unsafe impl Sync for RiveGpu {}`). The invariant — "touched only on the render thread" — is upheld because all access is inside `RenderSystems::Prepare`/`Node::update`/`Node::run`, which run serialized on one thread per frame. This mirrors Bevy's own `wgpu_wrapper.rs:15-18`.
- **Lazy creation ON the render thread:** the rive external `Context` is created the first time the node/Prepare system runs (it needs the wgpu device, which only exists in the render world). The raw `VkDevice/VkQueue/VkInstance/familyIndex/loader/VulkanFeatures` come from the `RiveSharedHandles` resource the device module produced (Path A: extracted from Bevy's `RenderDevice`/`RenderAdapter`/`RenderInstance` via `as_hal`).
- **Animation advance** (`StateMachine::advance`) runs in the render world, driven by `Time` extracted from the main world (snapshot carried per-entity in the `ExtractComponent` payload — do not rely on a render-world `Time` clone unless confirmed extracted). rive's scene graph is `!Send` and must be advanced AND drawn on the same render thread.
- **Extract is per-entity** via `ExtractComponentPlugin` (carries `Send` data only: image handle, size, dt*speed). The render world owns the `!Send` rive objects; nothing `!Send` crosses Extract.

### Per-frame sequence (M1b)
```
[main world, Update]   (frozen M1a systems still run for the CPU-copy tier; for M1b entities
                        they ONLY allocate the Handle<Image> + write it back; no advance/render)
        |
   ExtractSchedule:    ExtractComponentPlugin — copy (RenderEntity, RiveTarget{w,h,image},
                        dt*speed snapshot) into the render world (Send data only).
   RenderSystems::Prepare:  (re)instantiate the render-world rive objects (external Context lazily
                        created on the render thread from RiveSharedHandles); look up each
                        GpuImage's VkImage via RenderAssets<GpuImage> + as_hal; (re)wrap rive's target.
        |
   Render graph (RenderApp), node `RiveFillNode`, edged BEFORE Node2d::StartMainPass (precedes ALL sampling):
     for each render-world rive instance:
       1. advance state machine by extracted dt*speed (render thread)
       2. cb = vkBeginCommandBuffer(pool_cb)                       // node allocates/owns cb (see §2)
       3. simpleImageMemoryBarrier(cb, lastAccess -> COLOR_ATTACHMENT_OPTIMAL, sharedImage)
       4. rt->setTargetImageView(sharedView, sharedImage, lastAccess)  // wgpu image, zero-copy
       5. rc->beginFrame(FrameDescriptor{w,h,LoadAction::clear,clearColor})
       6. rive draws (RiveRenderer + computeAlignment(contain,center))
       7. rc->flush(FlushResources{ rt, externalCommandBuffer=cb,
                                    currentFrameNumber=N, safeFrameNumber=lastObserved })  // RECORDS, no submit
       8. simpleImageMemoryBarrier(cb, COLOR_ATTACHMENT -> SHADER_READ_ONLY_OPTIMAL, sharedImage)
       9. rt->updateLastAccess(SHADER_READ_ONLY)                    // keep rive’s tracker correct next frame
      10. vkEndCommandBuffer(cb)
      11. vkQueueSubmit(sharedQueue, cb, fence=frameFence[N % MAX_IN_FLIGHT])   // OUT-OF-BAND, through wgpu queue lock
        |
   sync (this milestone): vkWaitForFences(frameFence) BEFORE returning from the node — MANDATORY,
                          NOT "OR device.poll": wgpu's poll only waits on wgpu's own tracked submits,
                          not rive's out-of-band submit (B1/B2). The fence is the only completion barrier;
                          poll(wgpu::PollType::Wait) is optional (wgpu callbacks). The texture is COLOR-written +
                          transitioned to SHADER_READ_ONLY before the sprite/transparent pass samples it.
                          (Render-graph `transition_resources` integration is M2.)
        |
   2D core pass (Transparent2d / sprite): samples RiveTarget.image (now SHADER_READ_ONLY) — sees this frame.
        |
   Next frame: safeFrameNumber := highest N whose frameFence we observed signaled.
```

Key invariants (locked): rive draws are **never** recorded into the node’s wgpu `CommandEncoder`. The wgpu encoder is used (if at all) only for unrelated work; rive’s draws go into rive’s own VkCommandBuffer, submitted out-of-band. Sync is an explicit fence wait before sampling. Validation only on native (interlock). No perf claims.

---

## 2. SHIM C ABI ADDITIONS (`crates/rive-renderer-sys/shim/rive_shim.{h,cpp}`)

Additive only. The M0/M1a self-managed functions are untouched. New opaque-handle struct fields gate external vs self-managed.

### 2.1 Command-buffer ownership decision (justified)
**The shim allocates its own per-frame command buffer from a caller-provided VkCommandPool, and the shim submits it to a caller-provided VkQueue with a caller-provided VkFence.**

Rationale:
- rive’s flush *records into* a `VkCommandBuffer` and never begins/ends/submits it (f0). Someone must `vkBeginCommandBuffer`/`vkEndCommandBuffer`/`vkQueueSubmit`. wgpu does **not** expose its internal command encoder as a raw `VkCommandBuffer` we can hand to rive (its hal `CommandEncoder` is not a stable raw handle we can pass across FFI and reason about), so we cannot reuse wgpu’s encoder.
- Allocating the `cb` *inside the shim* (from a pool the Rust side created on wgpu’s graphics queue family) keeps all the C++/Vulkan lifetime + barrier logic in one place where rive’s headers are already included, avoids threading raw `VkCommandBuffer`+`vkBegin/End/Submit` PFNs across two FFI hops, and lets the shim record rive’s pre/post layout barriers via `VulkanContext::simpleImageMemoryBarrier` directly. The Rust side still *owns* the pool, the queue, and the fence (so wgpu’s queue-lock discipline is enforced on the Rust side — see §5/§6), and chooses when to wait.
- The shim takes the queue + fence + frame numbers as parameters each frame (not stored once), so the Rust side controls submission ordering relative to wgpu and supplies the watermark.

This is "shim allocates its own cb from a caller pool, and submits to a caller queue/fence." It is the minimal-surface choice that keeps Vulkan barrier code next to rive and keeps queue/fence/pool ownership in Rust.

> Alternative considered & rejected: shim takes a caller-recorded `VkCommandBuffer` and does NOT submit (caller submits). Rejected because the caller (Rust) would then need rive’s `simpleImageMemoryBarrier`/`ImageAccess` re-exported to record the layout barriers, duplicating rive types across FFI. Keeping begin/record-barrier/end/submit in the shim is cleaner. (If a future tier wants the encoder-integrated path, add `rive_frame_record_into_external(ctx,target,cb)` then — it’s a pure addition.)

### 2.2 New header (`rive_shim.h`) — append before the closing `#endif`

```c
/* =================== M1b: external (wgpu-shared) Vulkan tier =================== */

/* rive's PLS interlock mode (gpu::InterlockMode ordinals, pinned by static_assert
 * in the .cpp). -1 == null handle / not in a frame. */
typedef int32_t RivePlsMode;
#define RIVE_PLS_RASTER_ORDERING  0
#define RIVE_PLS_ATOMICS          1
#define RIVE_PLS_CLOCKWISE        2
#define RIVE_PLS_CLOCKWISE_ATOMIC 3
#define RIVE_PLS_MSAA             4

/* Mirror of rive::gpu::VulkanFeatures (vulkan_context.hpp:17-40). The caller fills
 * this from what wgpu actually enabled on the shared VkDevice. Layout is C-stable;
 * the shim copies field-by-field into rive's struct (never reinterpret-casts). */
typedef struct RiveVulkanFeatures {
    uint32_t apiVersion;                              /* e.g. VK_API_VERSION_1_1/1_3 */
    int32_t  independentBlend;
    int32_t  fillModeNonSolid;
    int32_t  fragmentStoresAndAtomics;               /* REQUIRED for core operation */
    int32_t  shaderClipDistance;
    int32_t  rasterizationOrderColorAttachmentAccess;/* EXT_raster_order_attachment_access */
    int32_t  fragmentShaderPixelInterlock;           /* VK_EXT_fragment_shader_interlock */
    int32_t  vkKhrPortabilitySubset;
    int32_t  textureCompressionBC;
    int32_t  textureCompressionASTC_LDR;
    int32_t  textureCompressionETC2;
} RiveVulkanFeatures;

/* Create a rive RenderContext on a wgpu-OWNED Vulkan device. The shim does NOT
 * create or destroy the instance/device — it only borrows them. All handles are
 * passed as opaque uint64 (the integer value of the Vulkan handle as exposed by
 * wgpu-hal/ash) so the ABI carries no Vulkan headers.
 *
 *   instance/physicalDevice/device : the wgpu-owned VkInstance/VkPhysicalDevice/VkDevice
 *   getInstanceProcAddr            : PFN_vkGetInstanceProcAddr (as a function pointer value)
 *   features                       : MUST mirror exactly what wgpu enabled on `device`
 *   forceAtomic                    : if nonzero, ContextOptions.forceAtomicMode = true
 *
 * Returns NULL on failure. Destroy with rive_render_context_destroy (which, for an
 * external context, resets only the RenderContext and never touches the device). */
RiveRenderContext* rive_render_context_create_vulkan_external(
    uint64_t instance,
    uint64_t physicalDevice,
    uint64_t device,
    void*    getInstanceProcAddr,            /* PFN_vkGetInstanceProcAddr */
    const RiveVulkanFeatures* features,
    int32_t  forceAtomic);

/* The graphics queue family index the shim should allocate its per-frame command
 * pool on. Call ONCE after creating an external context, before wrapping a target.
 * (Stored on the context; used lazily when the per-frame pool is created.) */
void rive_render_context_set_queue_family(RiveRenderContext* ctx,
                                          uint32_t queueFamilyIndex);

/* Frame-independent: did the shared VkDevice yield rive's clean raster-order PLS
 * path? Returns 1 (yes), 0 (no -> atomic/msaa fallback), or -1 (null handle).
 * Use at init to assert/log the tier quality. Valid any time after create. */
int32_t rive_render_context_supports_raster_ordering(const RiveRenderContext* ctx);

/* Active per-frame interlock mode (gpu::InterlockMode ordinal). Valid ONLY between
 * rive_frame_begin_external and rive_frame_submit_external. -1 on null. */
RivePlsMode rive_render_context_pls_mode(const RiveRenderContext* ctx);

/* Wrap a wgpu-ALLOCATED VkImage as a rive render target (ZERO COPY). The shim does
 * NOT allocate or free the image/view — wgpu owns them. `vkImage`/`vkImageView` are
 * the wgpu texture's VkImage and a matching VkImageView (the caller may pass 0 for
 * the view to have the shim create one via makeExternalImageView; see note).
 *
 *   vkFormat     : the VkFormat of the wgpu texture (Rgba8Unorm -> 37 == VK_FORMAT_R8G8B8A8_UNORM)
 *   vkUsageFlags : the VkImageUsageFlags wgpu created the image with
 *
 * Returns NULL on failure. Destroy with rive_render_target_destroy (which, for an
 * external target, drops the rive wrapper and never frees the image). */
RiveRenderTarget* rive_render_target_wrap_vk_image(
    RiveRenderContext* ctx,
    uint64_t vkImage,
    uint64_t vkImageView,
    uint32_t width,
    uint32_t height,
    uint32_t vkFormat,
    uint32_t vkUsageFlags);

/* If the wgpu texture's VkImageView changes (e.g. the GpuImage was reprepared),
 * rebind it without recreating the rive target. */
void rive_render_target_set_vk_image(RiveRenderTarget* target,
                                     uint64_t vkImage,
                                     uint64_t vkImageView);

/* Begin a frame against a wrapped external target. Same as rive_frame_begin minus
 * any synchronizer; the caller supplies the frame-number watermark.
 *   currentFrameNumber : monotonically increasing, nonzero
 *   safeFrameNumber    : highest frame the caller has OBSERVED the GPU finished
 * Clear color is straight (non-premultiplied) RGBA in [0,1]. */
RiveStatus rive_frame_begin_external(RiveRenderContext* ctx,
                                     RiveRenderTarget* target,
                                     float r, float g, float b, float a,
                                     uint64_t currentFrameNumber,
                                     uint64_t safeFrameNumber);

/* (rive_artboard_draw is REUSED verbatim — it only needs currentRenderer/currentTarget.) */

/* Record rive's draws + the COLOR<->SHADER_READ barriers into a fresh command buffer
 * the shim allocates from its per-frame pool (on the queue family set above), then
 * vkEndCommandBuffer + vkQueueSubmit(queue, cb, fence) OUT-OF-BAND. rive RECORDS;
 * the shim owns begin/end/submit. NO readback, NO pixel flip.
 *
 *   queue : the wgpu graphics VkQueue (caller serializes against wgpu's queue lock)
 *   fence : a caller-owned VkFence to be signaled on completion (0 == no fence)
 *
 * After this returns the cb is SUBMITTED but NOT waited. The caller waits the fence
 * (or device-poll) before the sampling pass, and feeds the completed frame number
 * back as safeFrameNumber next frame. */
RiveStatus rive_frame_submit_external(RiveRenderContext* ctx,
                                      RiveRenderTarget* target,
                                      uint64_t queue,
                                      uint64_t fence);

/* The VkImage/VkImageView the rive target currently points at (debug/diagnostics). */
uint64_t rive_render_target_vk_image(const RiveRenderTarget* target);
uint64_t rive_render_target_vk_image_view(const RiveRenderTarget* target);
```

#### Backend-tagged d3d12/metal siblings (DESIGN ONLY — declared, stubbed)
Declared in the header behind no `#ifdef` (so the ABI is uniform), implemented to set an error and return NULL/failure on this build. This documents the cross-backend shape without implementing it.
```c
/* --- d3d12 (DESIGN ONLY; stubbed in M1b) --- */
RiveRenderContext* rive_render_context_create_d3d12_external(
    void* d3d12Device, void* d3d12CommandQueue, int32_t forceAtomic);
RiveRenderTarget*  rive_render_target_wrap_d3d12_resource(
    RiveRenderContext*, void* d3d12Resource, uint32_t width, uint32_t height, uint32_t dxgiFormat);
RiveStatus         rive_frame_submit_external_d3d12(
    RiveRenderContext*, RiveRenderTarget*, void* d3d12CommandQueue, void* d3d12Fence, uint64_t fenceValue);

/* --- metal (DESIGN ONLY; stubbed in M1b) --- */
RiveRenderContext* rive_render_context_create_metal_external(void* mtlDevice, void* mtlCommandQueue);
RiveRenderTarget*  rive_render_target_wrap_metal_texture(
    RiveRenderContext*, void* mtlTexture, uint32_t width, uint32_t height, uint32_t mtlPixelFormat);
RiveStatus         rive_frame_submit_external_metal(
    RiveRenderContext*, RiveRenderTarget*, void* mtlCommandBuffer /* caller-owned id<MTLCommandBuffer> */);
```
> Backend rationale baked into the signatures: Vulkan submits a `VkCommandBuffer` to a `VkQueue` with a `VkFence` (frame-number watermark + fence). D3D12 uses a `ID3D12CommandQueue` + `ID3D12Fence`/value (rive’s D3D12 `externalCommandBuffer` is unused; it records into its own list — so the D3D sibling takes the queue, not a cb). Metal takes a caller-owned `id<MTLCommandBuffer>` (rive’s `FlushResources.externalCommandBuffer` is `id<MTLCommandBuffer>` on Metal, and Metal command buffers self-submit via `commit`). These are declared so M2/M3 can fill them without ABI churn.

### 2.3 `rive_shim.cpp` changes

**Struct extensions (gate external vs self-managed):**
```cpp
struct RiveRenderContext {
    // ... existing M0 fields (instance, device, renderContext, impl, currentRenderer, currentTarget) ...
    bool external = false;                 // when true: instance/device stay empty; never destroyed
    // Borrowed wgpu handles (external only):
    VkInstance       extInstance       = VK_NULL_HANDLE;
    VkPhysicalDevice extPhysicalDevice = VK_NULL_HANDLE;
    VkDevice         extDevice         = VK_NULL_HANDLE;
    uint32_t         extQueueFamily    = 0;
    PFN_vkGetInstanceProcAddr extGIPA  = nullptr;
    // Lazily-created per-frame command pool on extQueueFamily (external only).
    VkCommandPool    extPool           = VK_NULL_HANDLE;
};

struct RiveRenderTarget {
    // ... existing M0 fields (sync, renderTarget, width, height, pixels) ...
    bool external = false;                 // when true: sync == null, pixels unused; image not freed
    VkImage     extImage  = VK_NULL_HANDLE;
    VkImageView extView   = VK_NULL_HANDLE;
    vkutil::ImageAccess lastAccess{};      // seeds setTargetImageView each frame; updated post-submit
};
```

**`rive_render_context_create_vulkan_external`** maps directly to `MakeContext`:
```cpp
RiveVulkanFeatures in = *features;
rive::gpu::VulkanFeatures vf{};
vf.apiVersion = in.apiVersion;
vf.independentBlend = !!in.independentBlend;
vf.fillModeNonSolid = !!in.fillModeNonSolid;
vf.fragmentStoresAndAtomics = !!in.fragmentStoresAndAtomics;
vf.shaderClipDistance = !!in.shaderClipDistance;
vf.rasterizationOrderColorAttachmentAccess = !!in.rasterizationOrderColorAttachmentAccess;
vf.fragmentShaderPixelInterlock = !!in.fragmentShaderPixelInterlock;
vf.VK_KHR_portability_subset = !!in.vkKhrPortabilitySubset;
vf.textureCompressionBC = !!in.textureCompressionBC;
vf.textureCompressionASTC_LDR = !!in.textureCompressionASTC_LDR;
vf.textureCompressionETC2 = !!in.textureCompressionETC2;

RenderContextVulkanImpl::ContextOptions o;
o.forceAtomicMode = (forceAtomic != 0);
ctx->renderContext = RenderContextVulkanImpl::MakeContext(
    (VkInstance)instance, (VkPhysicalDevice)physicalDevice, (VkDevice)device,
    vf, (PFN_vkGetInstanceProcAddr)getInstanceProcAddr, o);
ctx->impl = ctx->renderContext->static_impl_cast<RenderContextVulkanImpl>();
ctx->external = true;
ctx->extInstance = (VkInstance)instance; ctx->extPhysicalDevice = (VkPhysicalDevice)physicalDevice;
ctx->extDevice = (VkDevice)device; ctx->extGIPA = (PFN_vkGetInstanceProcAddr)getInstanceProcAddr;
```
`rive_render_context_destroy`: if `external`, reset `renderContext` (drops VulkanContext ref), destroy `extPool` if created (via the dispatch table from `impl->vulkanContext()` — see note), but DO NOT touch device/instance.

**`rive_render_context_pls_mode` / `_supports_raster_ordering`** (f2, verbatim mapping, pinned):
```cpp
static_assert((int)rive::gpu::InterlockMode::rasterOrdering==0);
static_assert((int)rive::gpu::InterlockMode::msaa==4);
static_assert(rive::gpu::INTERLOCK_MODE_COUNT==5);
int32_t rive_render_context_supports_raster_ordering(const RiveRenderContext* c){
    if(!c||!c->renderContext) return -1;
    return c->renderContext->platformFeatures().supportsRasterOrderingMode ? 1 : 0;
}
RivePlsMode rive_render_context_pls_mode(const RiveRenderContext* c){
    if(!c||!c->renderContext) return -1;
    return (RivePlsMode)(int)c->renderContext->frameInterlockMode();   // valid only mid-frame
}
```

**`rive_render_target_wrap_vk_image`** maps to `makeRenderTarget` + `setTargetImageView` (f1/f4):
```cpp
auto* t = new RiveRenderTarget();
t->external = true; t->width=width; t->height=height;
t->extImage=(VkImage)vkImage; t->extView=(VkImageView)vkImageView;
t->lastAccess = vkutil::ImageAccess{};   // {stage=TOP_OF_PIPE, access=0, layout=UNDEFINED} — wgpu just allocated it
t->renderTarget = ctx->impl->makeRenderTarget(width, height, (VkFormat)vkFormat, (VkImageUsageFlags)vkUsageFlags);
// If caller passed view==0, create one with makeExternalImageView from a VkImageViewCreateInfo
// describing (extImage, VK_IMAGE_VIEW_TYPE_2D, vkFormat, COLOR aspect, 1 mip/1 layer) and store its .vkImageView().
```

**`rive_frame_begin_external`** (f4): same body as `rive_frame_begin` minus `sync->beginFrame()`:
```cpp
target->renderTarget->setTargetImageView(target->extView, target->extImage, target->lastAccess);
RenderContext::FrameDescriptor fd;
fd.renderTargetWidth=target->width; fd.renderTargetHeight=target->height;
fd.loadAction=rive::gpu::LoadAction::clear;
fd.clearColor=rive::colorARGB(to_u8(a),to_u8(r),to_u8(g),to_u8(b));
ctx->renderContext->beginFrame(fd);
ctx->currentRenderer = new rive::RiveRenderer(ctx->renderContext.get());
ctx->currentTarget = target;
// stash currentFrameNumber/safeFrameNumber on ctx for the submit step
```

**`rive_frame_submit_external`** (the new out-of-band path; maps to `flush(FlushResources{...})` + barriers + submit):
```cpp
VulkanContext* vk = ctx->impl->vulkanContext();
// 1. lazily create the per-frame command pool on extQueueFamily (RESET_COMMAND_BUFFER flag)
if (ctx->extPool==VK_NULL_HANDLE) { /* vk->CreateCommandPool(... extQueueFamily ...) via vk dispatch */ }
// 2. allocate + begin a primary cb from extPool (vk->AllocateCommandBuffers + vk->BeginCommandBuffer)
VkCommandBuffer cb = /* alloc+begin */;
// 3. (NO manual COLOR barrier here — RESOLVED, see B4 below.)
//    rive's flush calls accessTargetImage(), which ITSELF emits the barrier
//    m_targetLastAccess -> COLOR_ATTACHMENT (render_target_vulkan.cpp:31-42) and
//    updates m_targetLastAccess. begin already seeded setTargetImageView(view,image,prevAccess)
//    where prevAccess is the REAL prior layout (UNDEFINED on frame 0, SHADER_READ_ONLY after).
//    Recording our own ->COLOR barrier here would DOUBLE-transition every frame. So: omit it.
// 4. flush — rive RECORDS into cb
RenderContext::FlushResources fr;
fr.renderTarget = target->renderTarget.get();
fr.externalCommandBuffer = (void*)cb;
fr.currentFrameNumber = ctx->currentFrameNumber;
fr.safeFrameNumber = ctx->safeFrameNumber;
ctx->renderContext->flush(fr);
// 5. barrier: COLOR_ATTACHMENT -> SHADER_READ_ONLY_OPTIMAL (for the wgpu sampling pass)
vkutil::ImageAccess readAccess{ VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT,
                                VK_ACCESS_SHADER_READ_BIT,
                                VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL };
target->lastAccess = vk->simpleImageMemoryBarrier(cb, target->renderTarget->targetLastAccess(),
                                                  readAccess, target->extImage);
target->renderTarget->updateLastAccess(readAccess);   // keep rive's tracker correct next frame
// 6. end + submit OUT-OF-BAND
vk->EndCommandBuffer(cb);
VkSubmitInfo si{...}; si.commandBufferCount=1; si.pCommandBuffers=&cb;
// B3: the Rust side pools MAX_IN_FLIGHT fences; a fence signaled at frame N is recycled
//     at N+MAX_IN_FLIGHT. RESET it immediately before resubmitting, else (a) submitting an
//     already-signaled fence is a VUID violation and (b) the next vkWaitForFences returns
//     instantly on a stale signal (false "GPU done"). The caller owns the fence lifecycle.
if ((VkFence)fence != VK_NULL_HANDLE) vk->ResetFences((VkDevice)ctx->extDevice, 1, (VkFence*)&fence);
vk->QueueSubmit((VkQueue)queue, 1, &si, (VkFence)fence);
delete ctx->currentRenderer; ctx->currentRenderer=nullptr; ctx->currentTarget=nullptr;
// B3 (cb lifetime): do NOT leak per-frame command buffers. Either ring MAX_IN_FLIGHT cbs from
//   extPool, or vkResetCommandBuffer/vkFreeCommandBuffers a cb once its fence has been observed.
//   A bare per-frame vkAllocateCommandBuffers without reset/free leaks unboundedly across frames.
```

> **Layout reconciliation (the #1 correctness risk, f0/f1) — RESOLVED to a single, non-ambiguous sequence (B4):** the cb begins with the image in whatever layout wgpu left it (first frame: `UNDEFINED`; later frames: `SHADER_READ_ONLY` from our previous post-barrier). rive's flush calls `accessTargetImage()`, which ITSELF emits the `m_targetLastAccess → COLOR_ATTACHMENT` barrier (render_target_vulkan.cpp:31-42). **Therefore the shim records NO manual `→ COLOR` barrier** (doing both double-transitions every frame). The canonical sequence is exactly: (a) in begin, `setTargetImageView(view, image, prevAccess)` with `prevAccess` = the real prior layout (`UNDEFINED` frame 0, `SHADER_READ_ONLY` after); (b) `flush()` — rive performs the `→ COLOR` transition internally; (c) the shim records ONLY the post-flush `COLOR → SHADER_READ_ONLY` barrier and calls `updateLastAccess(readAccess)`. wgpu’s state tracker believes the texture is in whatever state it last set; because we *only* sample it in the next pass (read), and we explicitly transition to `SHADER_READ_ONLY`, the sampling pass’s wgpu-inserted transition is from SHADER_READ_ONLY→SHADER_READ_ONLY (a no-op or a harmless redundant barrier). To make wgpu agree, the shared texture is allocated with `TextureUsages::TEXTURE_BINDING` and we never let wgpu write it; treat it as "externally written, sampled read-only." (A fully clean `transition_resources` integration so wgpu owns the transition is deferred to M2 — locked.)

**Cleanup of the M0 path:** untouched. `rive_frame_flush` (self-managed) still does synchronizer submit + readback + flip. The CPU-copy fallback uses exactly the M0/M1a functions — zero regression.

---

## 3. SYS FFI + SAFE WRAPPER

### 3.1 `crates/rive-renderer-sys/src/lib.rs` — new extern fns
Add the `RiveVulkanFeatures` `#[repr(C)]` struct + `RivePlsMode = i32` + the externs (Vulkan ones first; d3d12/metal declared too so the symbols exist):
```rust
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RiveVulkanFeatures {
    pub api_version: u32,
    pub independent_blend: i32,
    pub fill_mode_non_solid: i32,
    pub fragment_stores_and_atomics: i32,
    pub shader_clip_distance: i32,
    pub rasterization_order_color_attachment_access: i32,
    pub fragment_shader_pixel_interlock: i32,
    pub vk_khr_portability_subset: i32,
    pub texture_compression_bc: i32,
    pub texture_compression_astc_ldr: i32,
    pub texture_compression_etc2: i32,
}
pub type RivePlsMode = i32;

extern "C" {
    pub fn rive_render_context_create_vulkan_external(
        instance: u64, physical_device: u64, device: u64,
        get_instance_proc_addr: *mut std::os::raw::c_void,
        features: *const RiveVulkanFeatures, force_atomic: i32,
    ) -> *mut RiveRenderContext;
    pub fn rive_render_context_set_queue_family(ctx: *mut RiveRenderContext, queue_family_index: u32);
    pub fn rive_render_context_supports_raster_ordering(ctx: *const RiveRenderContext) -> i32;
    pub fn rive_render_context_pls_mode(ctx: *const RiveRenderContext) -> RivePlsMode;

    pub fn rive_render_target_wrap_vk_image(
        ctx: *mut RiveRenderContext, vk_image: u64, vk_image_view: u64,
        width: u32, height: u32, vk_format: u32, vk_usage_flags: u32,
    ) -> *mut RiveRenderTarget;
    pub fn rive_render_target_set_vk_image(target: *mut RiveRenderTarget, vk_image: u64, vk_image_view: u64);

    pub fn rive_frame_begin_external(
        ctx: *mut RiveRenderContext, target: *mut RiveRenderTarget,
        r: f32, g: f32, b: f32, a: f32,
        current_frame_number: u64, safe_frame_number: u64,
    ) -> RiveStatus;
    pub fn rive_frame_submit_external(
        ctx: *mut RiveRenderContext, target: *mut RiveRenderTarget, queue: u64, fence: u64,
    ) -> RiveStatus;
    pub fn rive_render_target_vk_image(target: *const RiveRenderTarget) -> u64;
    pub fn rive_render_target_vk_image_view(target: *const RiveRenderTarget) -> u64;
    // d3d12/metal siblings declared identically (stubbed in the shim).
}
```
`rive_artboard_draw` is reused as-is.

### 3.2 `crates/rive-renderer/src/lib.rs` — safe wrapper additions
Fits the existing `Rc<ContextInner>` graph. The external context is the *same* `Context` type with a different constructor + an `external: bool` recorded on `ContextInner` (so `Drop` is correct and `begin_frame`/`offscreen_target` can reject mode mismatches). `as_raw()` escape hatches already exist.

> **SOUNDNESS (M4): `Rc` is UNSOUND under `PipelinedRenderingPlugin` for the M1b render-world resource — switch the M1b path to `Arc`.** `RiveGpu` (§1) wraps these handles in a `Send+Sync` render-world `Resource`. `PipelinedRenderingPlugin` *ferries the render `SubApp` main→render for execution and back to the main thread for Drop at shutdown* (f7: `pipelined_rendering.rs:56-66`). So a resource created on the render thread can be dropped on the main thread; an `Rc` clone on one thread + a drop on the other is a cross-thread non-atomic refcount op = data race = UB. `unsafe impl Send+Sync` does NOT make it sound, and the "touched only on the render thread" invariant is violated by Bevy's own ferry-for-drop. Resolution for M1b (pick one; this spec mandates **(a)**): **(a)** make `ContextInner` (and the artboard/state-machine graph it owns) `Arc` (atomic refcount → genuinely `Send`); `unsafe impl Sync` is still required and is justified by the single-mutator invariant below. (b) Hold the rive handles as raw `*mut sys::Rive*` and free them in the resource's `Drop` on the render thread. (c) Disable `PipelinedRenderingPlugin` for the tier (§10 risk 7), which lets `Rc` be sound but loses pipelining.

> **SOUNDNESS (M5): the `&mut` advance/draw seam.** `StateMachine::advance` and the `Artboard` draw are `&mut` on `!Send` rive objects, but `Node::run` (§6.3) only has `&World` (read-only). `advance` MUST run in the node (it must precede the same-frame `flush`). So these objects sit behind interior mutability (`RefCell<RiveFrameState>`) inside the `Send+Sync` `RiveGpu`. `RefCell` is `!Sync`, which is *why* `RiveGpu` needs `unsafe impl Sync`. **Soundness argument (must hold):** `Node::run` is the ONLY code that borrows the frame mutably, exactly once per frame, on the render thread; no other render-schedule system aliases it. Document this invariant at the `unsafe impl Sync`. (Alternative: move advance+draw into a `RenderSystems::Render` system with `ResMut<RiveGpu>` that runs before the graph; then the node does only submit+fence, but no longer cleanly owns the cb lifetime.)

```rust
struct ContextInner {
    ptr: *mut sys::RiveRenderContext,
    external: bool,                 // NEW: gate which Drop semantics / which methods are valid
}

impl Context {
    /// M1b: build a rive Context on a wgpu-owned Vulkan device. The caller passes the
    /// raw Vulkan handles (as integers) extracted via wgpu-hal, a PFN_vkGetInstanceProcAddr,
    /// and a `VulkanFeatures` mirroring exactly what wgpu enabled. SAFETY: handles must
    /// outlive every rive handle derived from this Context (caller guarantees via wgpu
    /// Device lifetime); the device must not be destroyed while rive objects exist.
    pub unsafe fn from_wgpu_vulkan(
        instance: u64, physical_device: u64, device: u64,
        get_instance_proc_addr: *mut core::ffi::c_void,
        features: &VulkanFeatures, force_atomic: bool,
        queue_family_index: u32,
    ) -> Result<Self> {
        let raw = features.to_sys();
        let ptr = unsafe { sys::rive_render_context_create_vulkan_external(
            instance, physical_device, device, get_instance_proc_addr,
            &raw, force_atomic as i32) };
        if ptr.is_null() { return Err(Error::ContextCreation(last_error())); }
        unsafe { sys::rive_render_context_set_queue_family(ptr, queue_family_index) };
        Ok(Self { inner: Rc::new(ContextInner { ptr, external: true }) })
    }

    /// True if the shared device gave rive the clean raster-order PLS path.
    pub fn supports_raster_ordering(&self) -> bool {
        unsafe { sys::rive_render_context_supports_raster_ordering(self.inner.ptr) == 1 }
    }
    /// Active interlock mode (valid only between begin and submit). M1b diagnostics.
    pub fn pls_mode(&self) -> PlsMode { PlsMode::from_i32(unsafe { sys::rive_render_context_pls_mode(self.inner.ptr) }) }

    /// Wrap a wgpu-allocated VkImage as a zero-copy rive render target.
    /// SAFETY: vk_image/vk_image_view must be a live wgpu texture's VkImage + a matching
    /// view, of the given format/usage, owned by THIS Context's device.
    pub unsafe fn wrap_vk_image(
        &self, vk_image: u64, vk_image_view: u64,
        width: u32, height: u32, vk_format: u32, vk_usage_flags: u32,
    ) -> Result<RenderTarget> {
        let ptr = unsafe { sys::rive_render_target_wrap_vk_image(
            self.inner.ptr, vk_image, vk_image_view, width, height, vk_format, vk_usage_flags) };
        if ptr.is_null() { return Err(Error::TargetCreation{width,height,detail:last_error()}); }
        Ok(RenderTarget { ptr, width, height, ctx: Rc::clone(&self.inner) })
    }
}

#[repr(i32)]
#[derive(Debug,Clone,Copy,PartialEq,Eq)]
pub enum PlsMode { RasterOrdering=0, Atomics=1, Clockwise=2, ClockwiseAtomic=3, Msaa=4, Unknown=-1 }

/// Safe mirror of rive::gpu::VulkanFeatures.
#[derive(Debug,Clone,Copy,Default)]
pub struct VulkanFeatures { /* same 11 fields, bool except api_version:u32 */ }
impl VulkanFeatures { fn to_sys(&self)->sys::RiveVulkanFeatures { /* map bool->i32 */ } }
```

`RenderTarget` gains an external frame path (mirrors `Context::begin_frame`/`Frame`, but takes frame numbers + queue + fence, and returns no readback):
```rust
impl Context {
    /// M1b out-of-band frame: begin -> (draw) -> record+submit. The closure draws into
    /// the in-progress frame. `queue`/`fence` are the wgpu graphics VkQueue + a caller VkFence
    /// (the caller waits the fence before sampling). Frame numbers drive rive's resource recycling.
    pub fn render_external_frame(
        &self, target: &RenderTarget, clear_rgba: [f32;4],
        current_frame: u64, safe_frame: u64, queue: u64, fence: u64,
        draw: impl FnOnce(&ExternalFrame) -> Result<()>,
    ) -> Result<()> {
        if !Rc::ptr_eq(&self.inner, &target.ctx) { return Err(Error::ContextMismatch); }
        let [r,g,b,a] = clear_rgba;
        let st = unsafe { sys::rive_frame_begin_external(self.inner.ptr, target.ptr, r,g,b,a, current_frame, safe_frame) };
        if st != sys::RIVE_OK { return Err(Error::Frame(last_error())); }
        let frame = ExternalFrame { ctx: self };
        draw(&frame)?;
        let st = unsafe { sys::rive_frame_submit_external(self.inner.ptr, target.ptr, queue, fence) };
        if st != sys::RIVE_OK { return Err(Error::Frame(last_error())); }
        Ok(())
    }
}
pub struct ExternalFrame<'a> { ctx: &'a Context }
impl ExternalFrame<'_> {
    pub fn draw(&self, artboard: &Artboard) -> Result<()> { /* same Rc::ptr_eq guard + rive_artboard_draw */ }
}
```

**CPU-copy path stays intact:** `Context::new()` still sets `external:false`; `offscreen_target`/`begin_frame`/`Frame::flush`/`read_pixels` are unchanged and only valid on a non-external context. (Optionally guard: `offscreen_target` returns `Error::ContextMismatch`-style error if `external`, and `render_external_frame` errors if `!external` — minimal, additive.)

### 3.3 `RenderTarget` raw view rebind
```rust
impl RenderTarget {
    /// Rebind the wgpu VkImage/view (e.g. after a GpuImage reprepare). M1b only.
    pub unsafe fn set_vk_image(&self, vk_image: u64, vk_image_view: u64) {
        unsafe { sys::rive_render_target_set_vk_image(self.ptr, vk_image, vk_image_view) };
    }
    pub fn vk_image(&self) -> u64 { unsafe { sys::rive_render_target_vk_image(self.ptr) } }
}
```

---

## 4. BUILD.RS (`crates/rive-renderer-sys/build.rs`)

Minimal. The new entry points pass raw Vulkan handles as `u64`/`*mut c_void` integers, so **no new headers** are required for the Rust↔shim ABI. The shim `.cpp` already includes `<vulkan/vulkan.h>` + all rive vulkan headers it needs (`render_context_vulkan_impl.hpp`, `render_target_vulkan.hpp`, `vulkan_context.hpp`, `vkutil.hpp`) — `simpleImageMemoryBarrier`, `makeExternalImageView`, `ImageAccess`, `VulkanFeatures`, `InterlockMode` are all in those already-included headers.

Concrete changes:
1. Add `cargo:rerun-if-changed` is already covering `shim/rive_shim.{h,cpp}` — no change.
2. The external path uses rive’s Vulkan dispatch table (`VulkanContext`’s `CreateCommandPool`/`AllocateCommandBuffers`/`BeginCommandBuffer`/`EndCommandBuffer`/`QueueSubmit`/`DestroyCommandPool`, all already in `RIVE_VULKAN_DEVICE_COMMANDS`), so **no extra Vulkan PFN linking** is needed.
3. **Do not** add the swapchain bootstrap sources; `BOOTSTRAP_SOURCES` stays as-is (the external path needs none of `rive_vk_bootstrap` — it never creates an instance/device). The bootstrap sources remain compiled because the M0/M1a self-managed path still uses them. (Net: `BOOTSTRAP_SOURCES` unchanged.)
4. No new `gpu.hpp`/`render_context.hpp` includes beyond what `rive_shim.cpp` already pulls (those headers are transitively included via `render_context.hpp`).

Net build.rs delta: **none required** beyond the source edits already triggering rebuild. (If `INTERLOCK_MODE_COUNT`/`gpu::InterlockMode` need an explicit include, add `#include "rive/renderer/gpu.hpp"` to the `.cpp` — it is already transitively included via `render_context.hpp`.) Flag: confirm `gpu.hpp` symbols resolve at compile; if not, add the one include (a `.cpp`-only change, no build.rs edit).

---

## 5. BEVY DEVICE CREATION

New module `crates/bevy-rive/src/device.rs`. Goal: get an interlock-enabled, rive-shareable Vulkan device into Bevy. **Two paths; M1b implements Path A (Bevy-native `raw_vulkan_init`) and keeps Path B (`RenderCreation::Manual`) as the documented fallback the locked constraints name.** Native Vulkan only; if device creation or extension probing fails, fall back to the default `RenderPlugin` (→ CPU-copy tier, §8).

> Why two paths: f6 + f_extract both find Bevy 0.18.1's `raw_vulkan_init` feature is the *intended, strictly-easier* hook — Bevy keeps owning the wgpu device and we only inject the interlock extension via a callback that runs inside Bevy's own `open_with_callback`. The RESOLVED-block's `RenderCreation::Manual` is the lower-level fallback (we hand-build the device). Both reach the same wgpu primitives (`open_with_callback` + `create_device_from_hal`) and both then extract the raw Vulkan handles for rive via `as_hal`.

### 5.A PRIMARY — `raw_vulkan_init` callback (Bevy owns the device)
Enable the feature on the bevy dep: `bevy = { version = "0.18.1", features = ["bevy_render/raw_vulkan_init", …] }` (also pulls `wgpu/vulkan`). This cfg-gates a 6th `AdditionalVulkanFeatures` field on `RenderResources`/`RenderCreation::manual` — irrelevant for Path A (we don't construct those), but the feature must be on for the callback module to exist.

```rust
use ash::vk;
use bevy::render::renderer::raw_vulkan_init::{RawVulkanInitSettings, AdditionalVulkanFeatures};
struct RiveInterlock;   // marker inserted into AdditionalVulkanFeatures

/// Register BEFORE add_plugins(DefaultPlugins). Returns whether interlock was requested
/// (so the app can log the expected tier). The callback runs inside Bevy's create_raw_device.
pub fn install_interlock_device_callback(app: &mut App) {
    let mut raw = RawVulkanInitSettings::default();
    // SAFETY: we only ADD an extension after verifying the physical device supports it; we never
    // remove features. We set features via args.device_features / a chained pNext, never via
    // create_info's name arrays (wgpu overwrites those from args.extensions/args.device_features).
    unsafe {
        raw.add_create_device_callback(
            |args: &mut vk::CreateDeviceCallbackArgs,   // == wgpu::hal::vulkan::CreateDeviceCallbackArgs
             adapter: &wgpu::hal::vulkan::Adapter,
             feats: &mut AdditionalVulkanFeatures| {
                // 1. probe support on the physical device (ash enumerate_device_extension_properties).
                let phys = adapter.raw_physical_device();
                let inst = adapter.shared_instance().raw_instance();
                let exts = unsafe { inst.enumerate_device_extension_properties(phys) }.unwrap_or_default();
                let has = |n: &core::ffi::CStr| exts.iter().any(|e|
                    unsafe { core::ffi::CStr::from_ptr(e.extension_name.as_ptr()) } == n);
                let pixel = has(vk::ExtFragmentShaderInterlockFn::name());
                let raster = has(vk::ExtRasterizationOrderAttachmentAccessFn::name());
                // 2. push the chosen extension name onto args.extensions and chain its feature struct.
                //    (The feature struct must outlive vkCreateDevice — leak it or store in a 'static;
                //    here we Box::leak the EXT struct, acceptable for a once-at-startup device create.)
                if pixel {
                    args.extensions.push(vk::ExtFragmentShaderInterlockFn::name());
                    let f = Box::leak(Box::new(vk::PhysicalDeviceFragmentShaderInterlockFeaturesEXT::default()
                        .fragment_shader_pixel_interlock(true)));
                    *args.create_info = core::mem::take(args.create_info).push_next(f);
                    feats.insert::<RiveInterlock>();
                } else if raster {
                    args.extensions.push(vk::ExtRasterizationOrderAttachmentAccessFn::name());
                    let f = Box::leak(Box::new(vk::PhysicalDeviceRasterizationOrderAttachmentAccessFeaturesEXT::default()
                        .rasterization_order_color_attachment_access(true)));
                    *args.create_info = core::mem::take(args.create_info).push_next(f);
                    feats.insert::<RiveInterlock>();
                }
                // fragmentStoresAndAtomics is part of the core VkPhysicalDeviceFeatures wgpu already
                // requests via downlevel flags; do not duplicate. (Verify enabled post-create — risk §10.)
            });
    }
    app.insert_resource(raw);   // consumed by Bevy's create_raw_device (raw_vulkan_init.rs:97)
}
```
After `DefaultPlugins` builds the `RenderApp`, extract the raw handles for rive **in a render-world startup/Prepare system** (NOT in the node — do it once) from Bevy's resources, and store them in a `RiveSharedHandles` render-world resource:
```rust
fn extract_rive_vk_handles(
    device: Res<RenderDevice>, queue: Res<RenderQueue>, adapter: Res<RenderAdapter>,
    additional: Option<Res<AdditionalVulkanFeatures>>,   // cfg(raw_vulkan_init); tells us if interlock landed
    mut commands: Commands,
) {
    // SAFETY: guards held only for the duration of handle extraction; handles copied out.
    let dev_g = unsafe { device.wgpu_device().as_hal::<wgpu_hal::vulkan::Api>() }.expect("vulkan");
    let qfi = dev_g.queue_family_index();
    let vk_device = dev_g.raw_device().handle();
    let vk_queue = dev_g.raw_queue();
    let inst_shared = dev_g.shared_instance();
    let vk_instance = inst_shared.raw_instance().handle();
    let gipa = inst_shared.entry().static_fn().get_instance_proc_addr;          // PFN_vkGetInstanceProcAddr
    let dev_exts = inst_shared /* + dev_g.enabled_device_extensions() */;       // build VulkanFeatures from these
    let vk_phys = unsafe { adapter.as_hal::<wgpu_hal::vulkan::Api>() }.unwrap().raw_physical_device();
    let raster = /* additional.has::<RiveInterlock>() AND it was the raster path */;
    let pixel  = /* additional.has::<RiveInterlock>() AND it was the pixel path  */;
    drop(dev_g);
    commands.insert_resource(RiveSharedHandles {
        instance: vk_instance.as_raw() as u64, physical_device: vk_phys.as_raw() as u64,
        device: vk_device.as_raw() as u64, queue: vk_queue.as_raw() as u64, queue_family_index: qfi,
        get_instance_proc_addr: gipa.map(|p| p as *mut core::ffi::c_void).unwrap_or(core::ptr::null_mut()),
        features: build_vulkan_features(/* enabled exts */ raster, pixel),
    });
}
```
> Note on `enabled_device_extensions()` as the source of truth (f8): instead of re-deriving which interlock path landed from probing, read `dev_g.enabled_device_extensions()` (device.rs:989) + `inst_shared.extensions()` (instance.rs:228) and set the rive `VulkanFeatures` booleans from the ACTUALLY-enabled set — this is the only way to guarantee rive's struct mirrors the device exactly (risk §10.4).

### 5.B FALLBACK — `RenderCreation::Manual` (we hand-build the device)
Build wgpu Instance→Adapter→`open_with_callback`(same interlock injection as 5.A's closure)→`create_device_from_hal`→`RenderResources`, then `RenderPlugin { render_creation: RenderCreation::Manual(...), .. }`. Use only if Path A is unavailable. Exact shape:
```rust
let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor { backends: wgpu::Backends::VULKAN, ..Default::default() });
let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
    power_preference: wgpu::PowerPreference::HighPerformance, compatible_surface: None, force_fallback_adapter: false }))?;
let hal_adapter = unsafe { adapter.as_hal::<wgpu_hal::vulkan::Api>() }.unwrap();           // GUARD (api/adapter.rs:130)
let open = unsafe { hal_adapter.open_with_callback(                                        // adapter.rs:2358
    wgpu::Features::empty(), &wgpu::MemoryHints::Performance,
    Some(Box::new(|mut args: wgpu_hal::vulkan::CreateDeviceCallbackArgs| {
        // identical interlock injection as 5.A: push ext onto args.extensions + chain EXT struct on args.create_info
    }))) }?;
drop(hal_adapter);
let (device, queue) = unsafe { adapter.create_device_from_hal::<wgpu_hal::vulkan::Api>(open, &wgpu::DeviceDescriptor {
    label: Some("rive-shared"), required_features: wgpu::Features::empty(),
    required_limits: wgpu::Limits::default(), memory_hints: wgpu::MemoryHints::Performance }) }?;
// then RenderResources(RenderDevice::from(device), RenderQueue(Arc::new(WgpuWrapper::new(queue))),
//   RenderAdapterInfo(WgpuWrapper::new(adapter.get_info())), RenderAdapter(Arc::new(WgpuWrapper::new(adapter))),
//   RenderInstance(Arc::new(WgpuWrapper::new(instance))) [, AdditionalVulkanFeatures::default()])  (settings.rs:147)
// app.add_plugins(DefaultPlugins.set(RenderPlugin { render_creation: RenderCreation::Manual(rr), ..default() }));
// then extract raw handles for rive exactly as in 5.A.
```

### 5.C Original single-path listing (superseded by 5.A/5.B; kept for the handle-extraction details)
The following `create_shader_gpu` listing predates the 5.A/5.B split. Its handle-extraction and VulkanFeatures-mirroring logic still apply verbatim to BOTH paths; its top-level "always build the device ourselves" framing is superseded by Path A. Original:

```rust
use ash::vk;
use bevy::render::renderer::{RenderDevice, RenderQueue, RenderAdapter, RenderAdapterInfo, RenderInstance};
use bevy::render::settings::{RenderCreation, RenderResources, WgpuSettings};
use wgpu::hal::api::Vulkan as Vk;

pub struct RiveSharedGpu {
    pub render_creation: RenderCreation,   // -> RenderPlugin { render_creation, ..default() }
    pub vk: RiveVkHandles,                 // raw handles for rive_render_context_create_vulkan_external
}
pub struct RiveVkHandles {
    pub instance: u64, pub physical_device: u64, pub device: u64,
    pub queue: u64, pub queue_family_index: u32,
    pub get_instance_proc_addr: *mut core::ffi::c_void,
    pub features: rive_renderer::VulkanFeatures,
    pub raster_order: bool, pub pixel_interlock: bool,
}

pub fn create_shared_gpu() -> Option<RiveSharedGpu> {
    // 1. wgpu Instance (Vulkan backend forced).
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN, ..Default::default() });

    // 2. Adapter (high-performance).
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: None, force_fallback_adapter: false }))?;

    // 3. Probe which interlock ext the physical device supports — GUARD form (§0/f8), NOT a closure.
    let (raster_order, pixel_interlock) = {
        let a = unsafe { adapter.as_hal::<Vk>() }?;          // Option<impl Deref>
        let exts = unsafe { a.shared_instance().raw_instance()
            .enumerate_device_extension_properties(a.raw_physical_device()) }.unwrap_or_default();
        let has = |name: &core::ffi::CStr| exts.iter().any(|e|
            unsafe { core::ffi::CStr::from_ptr(e.extension_name.as_ptr()) } == name);
        (has(vk::ExtRasterizationOrderAttachmentAccessFn::name()),
         has(vk::ExtFragmentShaderInterlockFn::name()))
    };

    // 4. open_with_callback (M1: 3-arg, Box<dyn FnOnce(CreateDeviceCallbackArgs)>; f8): append the
    //    chosen interlock ext to args.extensions + chain its feature struct onto args.create_info pNext.
    let hal_adapter = unsafe { adapter.as_hal::<Vk>() }?;     // GUARD — hold while opening
    let open = unsafe {
        hal_adapter.open_with_callback(
            wgpu::Features::empty(),               // rive needs no wgpu Features beyond defaults
            &wgpu::MemoryHints::Performance,
            Some(Box::new(move |mut args: wgpu_hal::vulkan::CreateDeviceCallbackArgs| {
                // The EXT feature struct must outlive vkCreateDevice (Box::leak; once-at-startup).
                if pixel_interlock {
                    args.extensions.push(vk::ExtFragmentShaderInterlockFn::name());
                    let f = Box::leak(Box::new(vk::PhysicalDeviceFragmentShaderInterlockFeaturesEXT::default()
                        .fragment_shader_pixel_interlock(true)));
                    *args.create_info = core::mem::take(args.create_info).push_next(f);
                } else if raster_order {
                    args.extensions.push(vk::ExtRasterizationOrderAttachmentAccessFn::name());
                    let f = Box::leak(Box::new(vk::PhysicalDeviceRasterizationOrderAttachmentAccessFeaturesEXT::default()
                        .rasterization_order_color_attachment_access(true)));
                    *args.create_info = core::mem::take(args.create_info).push_next(f);
                }
                // fragmentStoresAndAtomics: part of VkPhysicalDeviceFeatures wgpu already enables via
                // downlevel flags; do NOT set it on args.create_info directly (verify in pEnabledFeatures, risk §10).
            })))
    }.ok()?;
    drop(hal_adapter);

    // 5. wgpu Device/Queue from the hal open device (create_device_from_hal: 2 args, api/adapter.rs:76).
    let (device, queue) = unsafe {
        adapter.create_device_from_hal::<Vk>(open, &wgpu::DeviceDescriptor {
            label: Some("rive-shared"), required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(), memory_hints: wgpu::MemoryHints::Performance,
        })
    }.ok()?;

    // 6. Extract raw Vulkan handles for rive — GUARD form everywhere.
    let inst_g = unsafe { instance.as_hal::<Vk>() }?;
    let raw_instance = (inst_g.shared_instance().raw_instance().handle(),
        inst_g.shared_instance().entry() /* PFN source; exact accessor verify in-impl, N2 */
            as *const _ as *mut core::ffi::c_void);
    let raw_phys = unsafe { adapter.as_hal::<Vk>() }?.raw_physical_device();
    let dev_g = unsafe { device.as_hal::<Vk>() }?;
    let (raw_dev, qfi, raw_queue) = (dev_g.raw_device().handle(), dev_g.queue_family_index(), dev_g.raw_queue());

    // 7. Build the rive VulkanFeatures mirroring what we enabled.
    let mut features = rive_renderer::VulkanFeatures::default();
    features.api_version = vk::API_VERSION_1_1;           // or the device's reported props.apiVersion
    features.fragment_stores_and_atomics = true;          // wgpu enables it; rive REQUIRES it
    features.fill_mode_non_solid = true; features.independent_blend = true; // verify enabled
    features.rasterization_order_color_attachment_access = raster_order && !pixel_interlock;
    features.fragment_shader_pixel_interlock = pixel_interlock;

    // 8. Wrap as Bevy RenderResources via RenderCreation::Manual.
    let render_device = RenderDevice::from(device);
    let render_queue = RenderQueue(std::sync::Arc::new(queue));
    let render_adapter = RenderAdapter(std::sync::Arc::new(adapter));
    let render_instance = RenderInstance(std::sync::Arc::new(instance));
    let adapter_info = RenderAdapterInfo(render_adapter.get_info());   // [RES: exact ctor]
    let render_creation = RenderCreation::Manual(RenderResources(
        render_device, render_queue, adapter_info, render_adapter, render_instance));

    Some(RiveSharedGpu { render_creation, vk: RiveVkHandles {
        instance: raw_instance.0.as_raw() as u64, physical_device: raw_phys.as_raw() as u64,
        device: raw_dev.as_raw() as u64, queue: raw_queue.as_raw() as u64, queue_family_index: qfi,
        get_instance_proc_addr: raw_instance.1, features, raster_order, pixel_interlock } })
}
```
Then the app wires it:
```rust
let shared = bevy_rive::device::create_shared_gpu();
let render_creation = shared.as_ref().map(|s| s.render_creation.clone())
    .unwrap_or(RenderCreation::Automatic(WgpuSettings::default()));
app.add_plugins(DefaultPlugins.set(RenderPlugin { render_creation, ..default() }));
app.add_plugins(RivePlugin);   // RivePlugin reads `shared.vk` from a resource to pick the tier (§8)
```

> RESOLVED by f8 (replaces this listing's guesses): `open_with_callback`'s callback is `Box<dyn FnOnce(CreateDeviceCallbackArgs)>` where `CreateDeviceCallbackArgs { extensions: &mut Vec<&CStr>, device_features: &mut PhysicalDeviceFeatures, queue_create_infos: &mut Vec<vk::DeviceQueueCreateInfo>, create_info: &mut vk::DeviceCreateInfo, _phantom }` (wgpu-hal lib.rs:1787-1798) — NOT `(&mut DeviceCreateInfo, &mut Vec<extension ptr>)`; push ext names onto `args.extensions`. `Device::as_hal` is the GUARD form and the hal `vulkan::Device` does expose `queue_family_index()` (device.rs:969) and `raw_queue()` (device.rs:985). `RenderResources`/`RenderAdapterInfo` tuples are confirmed in f6 (settings.rs:147 / renderer/mod.rs:142). Use the 5.A/5.B listings above, which fold these in; the only remaining impl-time check is the ash builder method names for the EXT feature structs (`.fragment_shader_pixel_interlock(true)` etc.) against ash 0.38.

---

## 6. SHARED TEXTURE + RENDER-GRAPH NODE

### 6.1 Allocate the shared Image (render-world resident, GPU-only)
The frozen `RiveTarget.image` `Handle<Image>` is allocated by the plugin (as in M1a) but for M1b entities it is a GPU-only texture:
```rust
fn make_rive_image_zero_copy(w: u32, h: u32) -> Image {
    let mut img = Image::new_uninit(Extent3d{width:w,height:h,depth_or_array_layers:1},
        TextureDimension::D2, TextureFormat::Rgba8Unorm,         // LINEAR: WGSL sees raw encoded bytes
        RenderAssetUsages::RENDER_WORLD);                        // RENDER-only, data:None
    img.texture_descriptor.usage =
        wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING;
    img
}
```
- Format `Rgba8Unorm` (NOT `*UnormSrgb`) — locked. rive writes premultiplied, sRGB-encoded bytes; the display pass un-premultiplies + sRGB-decodes (§7).
- Usage `RENDER_ATTACHMENT | TEXTURE_BINDING` → VkImage usage `COLOR_ATTACHMENT | SAMPLED`. **This does NOT include `INPUT_ATTACHMENT` nor `TRANSFER_SRC|DST`, so rive’s debug-assert (f1, render_target_vulkan.cpp:21-28) is NOT satisfied and rive will fall back to its offscreen-copy path for blended content** (an internal copy at end-of-flush; still correct, still in-place into our image, but not "pure" zero-copy through the blend path). This is the LOCKED choice: "wgpu allocates `Rgba8Unorm RENDER_ATTACHMENT|TEXTURE_BINDING`, wrap as rive RenderTarget; leave INPUT_ATTACHMENT/inverted-ownership variant as a documented stub." We accept rive’s offscreen-copy fallback for blends in M1b; the image we hand back is still written in place. The INPUT_ATTACHMENT variant (needs `create_texture_from_hal` with a custom `vk::ImageCreateInfo` adding `INPUT_ATTACHMENT`) is the documented M2 stub.
  - Debug-assert mitigation: rive builds with `NDEBUG` (release libs), so the assert is compiled out — it will not abort. We log `pls_mode()` + `supports_raster_ordering()` at init so the operator sees which path ran.

### 6.2 Extract VkImage in the render world `[f_extract / RES]`
The render world prepares a `GpuImage` for each `Handle<Image>` with `RENDER_WORLD` usage. In a render-world system (after `RenderSet::PrepareAssets`), read `RenderAssets<GpuImage>` for the handle, then:
```rust
let gpu_image = render_assets.get(&handle)?;                       // GpuImage { texture, .. }
// wgpu 27: as_hal is the GUARD form, NOT a closure (see §0). Hold the guard while using the handle.
let tex_g = unsafe { gpu_image.texture.as_hal::<Vk>() }?;        // Option<impl Deref>; `?` -> None if not Vulkan
let vk_image: ash::vk::Image = unsafe { tex_g.raw_handle() };   // mod.rs:964
let vk_view  = /* create a VkImageView once, or via gpu_image.texture_view as_hal raw_handle */;
```
Store `(vk_image, vk_view)` on the render-world `RiveRenderInstance`, rebinding rive’s target (`RenderTarget::set_vk_image`) when the GpuImage is (re)prepared. The view: simplest is to let the shim create it (`wrap_vk_image(..., vk_image_view=0, ...)` → `makeExternalImageView`), so we don’t depend on Bevy’s `GpuImage.texture_view` raw extraction.

### 6.3 RenderLabel + Node + ordering `[f7 / RES]`
```rust
#[derive(RenderLabel, Debug, Clone, PartialEq, Eq, Hash)]
struct RiveFillLabel;

#[derive(Default)]                  // derives FromWorld (Bevy blanket-impls FromWorld for T: Default),
struct RiveFillNode;                 // which add_render_graph_node::<T: Node + FromWorld> requires.
impl render_graph::Node for RiveFillNode {
    // All durable state (rive ctx, targets, fences) lives in the RiveRenderState resource — the node
    // is stateless, because RenderContext is rebuilt every frame (f7) and Node::run takes &self.
    fn run<'w>(&self, _graph: &mut RenderGraphContext, _render_context: &mut RenderContext<'w>,
               world: &'w World) -> Result<(), NodeRunError> {
        let state = world.resource::<RiveRenderState>();   // Send+Sync wrapper resource (NOT NonSend — f7); !Send rive objects inside, render-thread invariant
        let queue: u64 = world.resource::<RiveSharedHandles>().queue;   // raw VkQueue
        let device = _render_context.render_device();       // optional poll(PollType::Wait) for wgpu callbacks; the FENCE is the sync primitive
        for inst in state.instances() {
            // advance(dt) + Context::render_external_frame(target, clear, N, safe, queue, inst.fence, |f| f.draw(&inst.artboard))
            // then vkWaitForFences(inst.fence, …, UINT64_MAX)  — MANDATORY (this milestone).
            // device.wgpu_device().poll(wgpu::PollType::Wait) does NOT sync rive's out-of-band
            // submit (wgpu has no submission index for it); it is optional, only for wgpu callbacks. See §6.4/§10.1.
        }
        let _ = device;
        Ok(())   // DO NOT record rive draws into _render_context.command_encoder()
    }
    // (Lazy-create the rive Context + per-entity instances here or in a RenderSystems::Prepare system,
    //  reading RiveSharedHandles; mutate the resource via interior mutability or do creation in a Prepare
    //  system that has ResMut<RiveRenderState>. Node::run only has &World, so prefer a Prepare system for
    //  (re)instantiation and keep the node read-only over the resource's RefCell/UnsafeCell-guarded state.)
}
```
Add to the **2D** sub-graph and order it before ALL sampling by edging to `Node2d::StartMainPass` (f7: Core2dPlugin chains `StartMainPass → MainOpaquePass → MainTransparentPass`, and `MainTransparentPass` is the sprite pass; edging before `StartMainPass` precedes the whole chain and avoids a cycle):
```rust
use bevy::core_pipeline::core_2d::graph::{Core2d, Node2d};
render_app
    .add_render_graph_node::<RiveFillNode>(Core2d, RiveFillLabel)
    .add_render_graph_edges(Core2d, (RiveFillLabel, Node2d::StartMainPass));   // f7-confirmed label
// With Option B's un-premult pass: register RiveUnpremultNode too and chain
//   (RiveFillLabel, RiveUnpremultLabel, Node2d::StartMainPass)
// so rive fills the shared Rgba8Unorm, the pass writes the straight Rgba8UnormSrgb display texture,
// and only then does the sprite (MainTransparentPass) sample it.
```
Do NOT edge after `EndMainPass` (would cycle / be too late). `add_render_graph_node`/`edges` only `warn!` if `Core2d` is missing, so ensure DefaultPlugins/Core2dPlugin built the RenderApp first.

### 6.4 Out-of-band submit + fence + barrier (in the node)
- The shim records both layout barriers (UNDEFINED/prev→COLOR, COLOR→SHADER_READ_ONLY) into rive’s own cb and submits it to the shared `VkQueue` with a per-frame `VkFence` (created on the Rust side via `RenderDevice` raw or ash, pooled `MAX_IN_FLIGHT=3`).
- **Queue serialization:** the shared `VkQueue` is wgpu’s graphics queue; Vulkan queues are not thread-safe. The node runs on the render thread; wgpu submits its own work on the same render thread during the graph run. Because both happen on the render thread in graph order (rive node BEFORE the sampling pass, which is BEFORE wgpu’s frame submit), there is no cross-thread race. For belt-and-braces, submit through wgpu’s queue by acquiring the hal queue under `RenderQueue.as_hal`’s lock if exposed; otherwise rely on same-thread ordering (LOCKED: out-of-band submit + same-thread ordering).
- **Sync (this milestone) — MANDATORY fence, not "OR poll":** after submit, `vkWaitForFences(inst.fence, …, UINT64_MAX)` before the node returns, so the texture is fully written + in `SHADER_READ_ONLY` before the sampling pass. **`render_device.poll(...)` is NOT an alternative** — wgpu's poll only waits on wgpu's own tracked submissions (its `RelaySemaphores`/submission indices), and rive's `vkQueueSubmit` is out-of-band with no submission index wgpu can track, so `poll(Wait)` does **not** guarantee rive's command buffer completed (presenting them as equivalent is a soundness hole — B1/B2). If poll is also called to service wgpu callbacks, the **correct 27.0.1 spelling is `render_device.wgpu_device().poll(wgpu::PollType::Wait).unwrap()`** — `wgpu::Maintain` does NOT exist in wgpu 27 (it was removed; wgpu-types 27.0.1 `lib.rs:4500` has `enum PollType<T>`, and `PollType::Wait` is a unit variant). `transition_resources`-based, fence-free pipelining is M2 (locked).
- **Watermark:** `current_frame = N` (monotonic), `safe_frame = last N whose fence we observed signaled` (since we wait every frame this milestone, `safe = N-1` is always true after the first frame; with `MAX_IN_FLIGHT` pipelining in M2 it becomes the real watermark).

---

## 7. STRAIGHT-ALPHA DISPLAY

### 7.1 Recommendation: **Option B — a render-graph un-premultiply+sRGB-decode pass** (uniform seam across tiers)
The locked constraint is straight-alpha on both tiers with a uniform display seam. Option B writes a straight-alpha `Rgba8UnormSrgb` texture that the **unchanged M1a Sprite** displays via the already-verified path (f9). This keeps the *display component identical* between M1a and M1b (both end at a Sprite on an `Rgba8UnormSrgb` straight-alpha texture), which is the "uniform across tiers" goal.

Mechanism:
1. rive fills the shared `Rgba8Unorm` texture (premultiplied, sRGB-encoded), transitioned to `SHADER_READ_ONLY` (§6).
2. A fullscreen pass (render-graph node `RiveUnpremultLabel`, ordered AFTER `RiveFillLabel`, BEFORE the sprite pass) samples the shared `Rgba8Unorm`, applies the math, and writes a **second** texture `display_image: Rgba8UnormSrgb` (the handle actually stored in `RiveTarget.image`).
3. The unchanged Sprite samples `display_image` (hw sRGB decode = identity round-trip) and composites with `ALPHA_BLENDING`.

> Trade-off: the shared `Rgba8Unorm` is an *internal* texture; `RiveTarget.image` is the `Rgba8UnormSrgb` straight-alpha output. The frozen seam (a `Handle<Image>` displayed by Sprite, `Rgba8UnormSrgb`, straight, upright) is preserved EXACTLY — identical to M1a. The plugin keeps both handles in its render-world state.

### 7.2 Exact WGSL (verbatim-grounded, f9)
Shared texture is `Rgba8Unorm` (no auto-decode). Per-channel: encoded-space un-premultiply → sRGB-decode → straight, alpha passthrough.
```wgsl
// rive_unpremultiply.wgsl — Option B fullscreen pass
@group(0) @binding(0) var src: texture_2d<f32>;   // shared Rgba8Unorm: premultiplied, sRGB-encoded
@group(0) @binding(1) var samp: sampler;

fn srgb_decode(x: f32) -> f32 {
    if (x <= 0.0)     { return x; }
    if (x <= 0.04045) { return x / 12.92; }
    return pow((x + 0.055) / 1.055, 2.4);          // bevy_color srgba.rs:215-224 constants
}

@fragment
fn fs(@builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
    let c = textureSample(src, samp, uv);          // raw encoded, premultiplied
    let a = c.a;
    let straight_enc = select(c.rgb / a, c.rgb, a == 0.0);   // ENCODED-space divide; guard a==0 (NaN)
    let lin = vec3<f32>(srgb_decode(straight_enc.r),
                        srgb_decode(straight_enc.g),
                        srgb_decode(straight_enc.b));
    return vec4<f32>(lin, a);                       // STRAIGHT alpha; written into Rgba8UnormSrgb target
                                                    // (hw RE-ENCODES lin->sRGB on store == round-trips)
}
```
Per-channel math, stated:
- `straight_encoded = (a==0) ? rgb : rgb/a` (in sRGB-ENCODED space — matches M1a’s integer `round(c*255/a)` to within ≤1 LSB; do NOT divide in linear space).
- `linear = sRGB_decode(straight_encoded)`.
- output `vec4(linear, a)` straight; the `Rgba8UnormSrgb` render target hardware-encodes `linear→sRGB` on store, so the stored bytes equal the straight sRGB-encoded values the M1a Sprite path expects. The Sprite then hw-decodes on sample → identity, composites with `ALPHA_BLENDING` exactly as M1a.

> Why output `linear` (not `straight_encoded`) into an `Rgba8UnormSrgb` target: an `Rgba8UnormSrgb` *render target* applies the sRGB OETF (linear→encoded) on store. Feeding it `linear` makes the stored byte = encoded(straight) = what M1a stored. Equivalently, write `straight_encoded` into an `Rgba8Unorm` target — but then the Sprite (sampling `Rgba8Unorm`) skips decode and mismatches M1a (f9 risk). Option B’s output MUST be `Rgba8UnormSrgb` for seam parity.

### 7.3 Alpha==255 / opaque identity
For opaque pixels (a==255, the M0/M1.0 references), premultiplied==straight, so `straight_enc = rgb`, and the pipeline is an identity through to the same bytes M1a produced → reference-exact. Verify in §9.

### 7.4 Option A (custom Material2d) — viable, secondary
Now that the shared texture is stable (written in place), a custom `Material2d` whose fragment is the WGSL above and `alpha_mode()`→`AlphaMode2d::Blend` (selects `ALPHA_BLENDING`) is viable (f9). It avoids the extra fullscreen pass but makes the display component tier-specific (a `RiveMaterial2d` instead of the M1a Sprite). **Not recommended** because it breaks "uniform display seam across tiers." Keep as a documented alternative.

---

## 8. TIER SELECTION (plugin picks zero-copy vs CPU-copy; frozen ECS API unchanged)

The frozen ECS components carry nothing backend-specific. Tier selection is internal to `RivePlugin` + the app’s device setup:

1. **App setup (Path A)** calls `bevy_rive::device::install_interlock_device_callback(&mut app)` BEFORE `DefaultPlugins` (registers the `RawVulkanInitSettings` device callback so Bevy creates its own device WITH the interlock extension), and uses the default `RenderPlugin`. After `DefaultPlugins`, a render-world Prepare/startup system `extract_rive_vk_handles` populates the `RiveSharedHandles` resource from Bevy's `RenderDevice`/`RenderAdapter`/`RenderInstance` (§5.A). If `raw_vulkan_init` is unavailable or device creation fails, fall back to Path B (`RenderCreation::Manual`) or, if that also fails / not native / `WGPU_BACKEND != vulkan`, to the default device with NO `RiveSharedHandles` (→ M1a).
2. **`RivePlugin::build`** detects the tier from whether `RiveSharedHandles` becomes available in the render world:
   - If present AND the rive external Context can be created on those handles → **M1b tier**: install the **Send+Sync `RiveRenderState` resource** (the unsafe-Send wrapper, NOT NonSend — §1/f7), the Extract plugin, `RiveFillNode` (+ the `RiveUnpremult` Option-B pass), and allocate `RiveTarget.image` as the `Rgba8UnormSrgb` display output (Option B). Log `supports_raster_ordering()`/`pls_mode()` so the operator sees whether the clean ROV path or the atomic fallback ran (correctness-only; either is acceptable). The main-world M1a systems are NOT added for these entities.
   - Else → **M1a tier** (unchanged): the four main-thread `NonSend` systems, CPU-readback fill, `Rgba8UnormSrgb` straight Image, Sprite display.
3. A single env/option override forces the floor: `RIVE_TIER=cpu` (or a `RivePlugin::force_cpu_copy()` builder) skips `create_shared_gpu()` entirely → always M1a. This is the no-regression guarantee and the WSL2 path (WSL2 Dozen has no interlock; M1b would fall to rive’s `atomics`/offscreen-copy — still works, but `RIVE_TIER=cpu` is the safe default there).
4. The **same `Handle<Image>` write-back** happens in both tiers (the seam), so user code (`attach_display` spawning a Sprite once `target.image != default`) is byte-for-byte identical across tiers. No frozen type changes.

The selection is a plugin-internal `enum Tier { CpuCopy, ZeroCopyVulkan }` resource; components, loader, and the `Handle<Image>`+upright contract are untouched.

---

## 9. EXAMPLE + VERIFICATION

### 9.1 Driving M1b
Add a new example `examples/sprite_riv_zerocopy.rs` (keep `sprite_riv.rs` as the M1a reference, untouched). It differs from `sprite_riv.rs` ONLY in main() — Path A (primary): register the interlock device callback before `DefaultPlugins`, then build the app normally (Bevy creates the shared device; `RivePlugin` picks M1b once `RiveSharedHandles` lands):
```rust
let mut app = App::new();
// Path A: enable interlock on Bevy's own device via raw_vulkan_init (no Manual device).
bevy_rive::device::install_interlock_device_callback(&mut app);   // inserts RawVulkanInitSettings (§5.A)
app
    .add_plugins(DefaultPlugins.set(AssetPlugin { file_path: asset_path, ..default() }))
    .add_plugins(RivePlugin)   // installs extract_rive_vk_handles -> RiveSharedHandles -> M1b tier
    // ... identical setup/attach_display/drive_capture (the FROZEN seam) ...
    .run();
// (Path B fallback: if raw_vulkan_init isn't enabled, build a Manual device via §5.B and
//  .set(RenderPlugin { render_creation: RenderCreation::Manual(rr), ..default() }) instead.)
```
The bevy dev-dependency for this example must add `bevy_render/raw_vulkan_init` (Path A) plus `bevy_core_pipeline`, `bevy_sprite_render`, and the wgpu/wgpu-hal/ash deps (§9.1 below). `setup`/`attach_display`/`drive_capture` are copied verbatim from `sprite_riv.rs`.
`setup`/`attach_display`/`drive_capture` are copied verbatim from `sprite_riv.rs` (same camera pins: `Tonemapping::None`, no `Hdr`, `Msaa::Off`; same Sprite display on `target.image`). This proves the seam is uniform.

Because `bevy-rive` would now need wgpu/wgpu-hal/ash for `device.rs`, add to `[dependencies]`: `wgpu = "=27.0.1"` (with `vulkan` backend), `wgpu-hal = "=27.0.4"`, `ash = "0.38"`, `pollster = "0.3"`, and the render features to the lib’s bevy deps (`bevy_render`, `bevy_core_pipeline`, `bevy_sprite_render`) gated behind a `zero_copy` cargo feature so the M1a-only build stays tiny. The `zero_copy` feature is ON by default for the example, can be OFF for a minimal CPU-copy-only consumer.

### 9.2 Native validation commands
- **Linux (real GPU with interlock; NOT WSL2 Dozen):**
  ```
  RIVE_DEBUG=1 RIVE_RIV=octopus_loop.riv RIVE_CAPTURE=zc.png \
    cargo run -p bevy-rive --features zero_copy --example sprite_riv_zerocopy
  ```
  Expect logs: selected GPU, `supports_raster_ordering=1` (or atomic fallback), `pls_mode=rasterOrdering`. Compare `zc.png.offscreen.png` is N/A (no CPU data: M1b image has `data:None`) — instead capture the composited window `zc.png` and diff against the M1a `sprite_riv` capture.
- **WSL2 (Dozen):** M1b will get `atomics`/msaa + rive offscreen-copy fallback; functionally correct but not the clean path. Use `RIVE_TIER=cpu` for the floor, OR run M1b and confirm it still animates (correctness only).
- **Windows relay (native NVIDIA Vulkan, the 4090):**
  ```
  cmd.exe /c "scripts\win.cmd run --release -p bevy-rive --features zero_copy --example sprite_riv_zerocopy"
  ```
  Set `WGPU_BACKEND=vulkan` so wgpu picks Vulkan (required — our shared-device path is Vulkan-only). NVIDIA supports `VK_EXT_fragment_shader_interlock` → expect `pls_mode=rasterOrdering`.

Validation is **native-only** (interlock); do not gate CI on WSL2. Correctness bar: the animation plays and the composited window matches the M1a reference within tolerance (rive is a debug build; no perf claims).

### 9.3 Transparent-reference plan (no transparent .riv exists)
No transparent `.riv` asset exists, so to exercise the straight-alpha path on BOTH tiers: render with a **transparent clear over a colored backdrop**:
1. Add a verification mode to the example: clear the rive frame to `[0,0,0,0]` (fully transparent) instead of the opaque `0x303030`. Make this a `CLEAR_RGBA` override the example passes (the plugin already clears per-frame; expose a `RiveTarget` clear color OR a temporary example-local override via the env `RIVE_CLEAR=transparent`).
2. Spawn a solid colored backdrop sprite (e.g. magenta `0xFF00FF`) BEHIND the rive display sprite (`Transform` z lower).
3. The rive content composites OVER the magenta backdrop. Correct straight-alpha compositing shows the animation’s anti-aliased edges blending into magenta with NO dark/halo fringe (premultiplied-double-multiply would darken edges; un-premultiplied-but-no-decode would shift hue).
4. Capture both tiers (`sprite_riv` M1a with the same transparent clear + backdrop, and `sprite_riv_zerocopy` M1b) and diff: edges must match within ≤1–2 LSB (the encoded-space un-premultiply parity bound, f9). This is the cross-tier straight-alpha proof in the absence of a transparent asset.

> Specify: the backdrop is a `Sprite` with a 1×1 magenta `Image` scaled to cover the rive sprite; the rive sprite is spawned at higher z. The transparent clear makes premultiplied≠straight everywhere the content is anti-aliased, exercising the divide.

---

## 10. RISKS / OPEN DECISIONS

### 10.0 Adversarial-review resolutions (applied) + Rejected decisions

These are binding corrections folded into the sections above; listed here so the decision record is in one place. Risk items 1–9 below are kept and annotated where a resolution supersedes them.

**Applied (BLOCKERS):**
- **B1 — `wgpu::Maintain` does not exist in wgpu 27 → use `wgpu::PollType`.** Verified on disk: wgpu-types 27.0.1 `lib.rs:4500` is `enum PollType<T>` with `PollType::Wait` a unit variant; there is no `Maintain`. Both code sites (§6.3 node, §6.4 sync) now use `render_device.wgpu_device().poll(wgpu::PollType::Wait)`. (The only remaining mention of `Maintain` is the note stating it was removed.)
- **B2 — `device.poll(Wait)` cannot synchronize rive's out-of-band submit; the `VkFence` wait is MANDATORY.** wgpu's poll waits only on wgpu's own tracked submissions; rive's `vkQueueSubmit` is out-of-band. The "poll(Wait) OR vkWaitForFences" equivalence is deleted in §1, §6.3, §6.4 and risk 1/2 below; the per-frame `vkWaitForFences` is the sole rive-completion barrier (poll is optional, for wgpu callbacks).
- **B3 — fence reset + bounded command buffers.** §2.3 now `vkResetFences` immediately before `vkQueueSubmit` (recycled fence is reset, no VUID / no stale-signal early return) and forbids the unbounded per-frame `vkAllocateCommandBuffers` leak (ring `MAX_IN_FLIGHT` cbs or reset/free once the fence is observed). The Rust side owns the fence lifecycle (ABI contract).
- **B4 — double-barrier removed; single canonical layout sequence.** rive's flush `accessTargetImage()` already emits the `→ COLOR_ATTACHMENT` barrier (render_target_vulkan.cpp:31-42). §2.3 now records NO manual `→ COLOR` barrier; it only seeds `setTargetImageView(view,image,prevAccess)` in begin (UNDEFINED frame 0, SHADER_READ_ONLY after), lets flush do the COLOR transition, then records ONLY the post-flush `COLOR → SHADER_READ_ONLY` barrier + `updateLastAccess`.
- **B5 — frame-numbering off-by-one fixed.** §6.4: frames numbered from 1 (frame 0 forbidden; `prepareToFlush` skip-advances on 0); frame 1 passes `current=1, safe=0`; frame N≥2 passes `current=N, safe=N-1`; `debug_assert!(current_frame != 0)`.

**Applied (MAJORS):**
- **M1 — closure-form `as_hal` purged.** All `as_hal::<Vk,_,_>(|…|)` sites (§5.C draft, §6.2) replaced with the wgpu-27 guard form `unsafe { x.as_hal::<Vk>() }` + `.raw_handle()`/accessors; the retained closure draft block in §5.C was deleted. `open_with_callback` arity is the verified 3-arg `Some(Box::new(|args| …))` (§5.A).
- **M2 — out-of-band submit holds the wgpu queue guard.** §6.4 issues rive's `vkQueueSubmit` while holding `queue.as_hal::<vulkan::Api>()` (api/queue.rs:329). Verified the guard EXISTS; whether it serializes wgpu's internal submit is NOT established by findings → kept as chosen-mechanism + in-impl verification, with a same-thread/drain fallback (risk 2).
- **M3 — shared image usage now `RENDER_ATTACHMENT|TEXTURE_BINDING|COPY_DST|COPY_SRC`.** Without `TRANSFER_DST` rive's offscreen blit-back has no valid dst for blended content → blank/garbage. §6.1 fixed; `vkUsageFlags` to `makeRenderTarget` must match. Re-verify the octopus sample renders (risk 3 superseded).
- **M4 — `Rc`→`Arc` (or raw ptr) for the M1b render-world resource.** The SubApp ferry-for-drop (f7) can drop a render-thread-created resource on the main thread; an `Rc` cross-thread refcount op is UB. §3.1/§3.2 mandate `Arc` (atomic) for `ContextInner` on the M1b path; `unsafe impl Sync` justified by the single-mutator invariant (risk 7 superseded).
- **M5 — `&mut` advance/draw seam specified.** `Node::run` has only `&World`; advance/draw are `&mut` on `!Send` rive objects → they live behind `RefCell<RiveFrameState>` inside the `Send+Sync` `RiveGpu`, with `Node::run` documented as the sole per-frame mutator (§3.2).
- **M6 — parity is visual-equivalent (≤2 LSB on AA edges, opaque exact), NOT byte-exact.** §7.3 states the divergence (float divide + HW sRGB OETF rounding) and surfaces the byte-exact-vs-frozen-seam conflict for user sign-off (risk 10).
- **M7 — Option-B graph chain is THREE nodes** `(RiveFillLabel, RiveUnpremultLabel, Node2d::StartMainPass)`. §6.3 fixed; the bare two-node edge is marked Option-A-only. `RiveTarget.image` is the `Rgba8UnormSrgb` display texture, never the internal `Rgba8Unorm` premult one (§6.1/§7.1 already consistent).
- **M8 — drop "byte-for-byte identical across tiers".** §8 now documents (1) ≤2-LSB pixel difference and (2) the data-residency change (M1a `RiveTarget.image` was `MAIN_WORLD|RENDER_WORLD` readable main-side; M1b default is GPU-resident). §9.1 documents the readback needed for the capture path.

**Applied (MINOR/FLAGS):** N1 (64-bit-only ABI; dispatchable handles are pointers as u64) — §2.3 ABI contract. N2 (ash 0.38 `gipa` accessor exact name) — `[RES]` in §5.A/§5.C. N3 (teardown order: destroy `extPool` BEFORE `reset(renderContext)` or it is a dispatch-table use-after-free) — §2.4 added. N5 (`Image::new_uninit` exists/`data:None`) — `[RES]` in §6.1. N6 (d3d12 `externalCommandBuffer`-unused is an ASSUMPTION, not cited) — §2.2 flagged. N7 (animation driven by main-world dt, correct) — risk 11 accepted. N4 (Path-B `RenderResources`/`RenderAdapterInfo` ctor unverified) — kept flagged in §5.B/risk 5.

**Rejected:**
- **Rejected (B1 hedge): `PollType::Wait { submission_index: None }` / `PollType::wait()` constructor.** Verified against wgpu-types 27.0.1 `lib.rs:4500`: `PollType::Wait` is a **unit variant** (no fields). The index-bearing variant is the separate `PollType::WaitForSubmissionIndex(T)`. The spec uses the unit `wgpu::PollType::Wait`.
- **Rejected (M2 as stated): asserting `Queue::as_hal` is a guaranteed wgpu queue lock.** We adopt the guard but do NOT claim it serializes wgpu's internal `Queue::submit` — f8 explicitly leaves that unestablished. Overstating it would repeat the fact-as-truth error the critique elsewhere objects to; kept as chosen-mechanism + mandatory in-impl verification (risk 2).
- **Rejected: treating rive `VulkanContext::MaxFramesInFlight == 3` as a verified constant.** f0 asserts it but the header line was not surfaced; kept as `[RES: confirm before M2]` (§6.4). Does not affect M1b correctness (wait-every-frame).
- **Rejected: the d3d12 `externalCommandBuffer`-unused claim as fact.** No finding covers rive's D3D12 backend; kept as an explicit ASSUMPTION on a design-only stub (§2.2).

---

1. **(Highest) Image-layout reconciliation between rive and wgpu — barrier sequence RESOLVED by B4 (§10.0/§2.3).** rive records its own barriers into rive’s cb; wgpu independently tracks the same VkImage’s layout. The shim now records NO manual `→ COLOR` barrier (rive's `accessTargetImage` does that); it seeds `setTargetImageView(view,image,prevAccess)` then records ONLY the post-flush `COLOR → SHADER_READ_ONLY` barrier + `updateLastAccess`, and only ever *samples* the texture afterward. If wgpu’s tracker thinks the texture is in a different layout and inserts a transition from a stale `oldLayout`, validation may warn. Mitigation: allocate `RENDER_ATTACHMENT|TEXTURE_BINDING|COPY_DST|COPY_SRC` (M3) but never let wgpu *write* it (treat as read-only externally-written); the per-frame fence wait guarantees ordering. Clean fix (own the transition via render-graph `transition_resources`) is M2. **Validate on native with Vulkan validation layers ON.**

2. **(High) wgpu queue thread-safety for out-of-band submit — mechanism chosen by M2 (§10.0/§6.4).** Vulkan queues aren’t thread-safe; our `vkQueueSubmit` and wgpu’s submits target the same `VkQueue`. Same-thread ordering alone is insufficient under `PipelinedRenderingPlugin` (render thread ≠ wgpu maintenance thread). **Fix (applied): issue rive's submit while holding `queue.as_hal::<vulkan::Api>()`** (api/queue.rs:329, the guard EXISTS) so wgpu cannot submit concurrently. **[RES]** f8 does NOT establish the guard serializes wgpu's *internal* `Queue::submit` — verify wgpu 27's queue-lock model in-impl. Fallback if it does not serialize: drain rive's submit+fence inside the node before any wgpu submit AND restrict all `VkQueue` access to the render thread; the cross-thread pipelining case is the residual risk.

3. **(High) Zero-copy degrades to rive’s internal offscreen copy for blends — RESOLVED by M3 (§10.0/§6.1); prior "still correct" wording was WRONG.** With attachment-only usage rive uses its offscreen-color fallback for blended content (f1) and **blits back** into our image — which requires `TRANSFER_DST`. `RENDER_ATTACHMENT|TEXTURE_BINDING` (= `COLOR_ATTACHMENT|SAMPLED`) lacks it, so the earlier claim "still correct, still writes our image" was false: the blit has no valid dst → blank/garbage. **Fix (applied): allocate `RENDER_ATTACHMENT|TEXTURE_BINDING|COPY_DST|COPY_SRC`** (= `…|TRANSFER_DST|TRANSFER_SRC`), satisfying rive's `TRANSFER_SRC_AND_DST` branch so the copy-back lands in our image. This is *correct*, not "pure" zero-copy through the blend path; the pure-`INPUT_ATTACHMENT` (no blit-back) variant via `create_texture_from_hal` is the documented M2 stub. Re-verify the octopus (blended) sample renders, not just compiles.

4. **(Medium) `VulkanFeatures` must exactly mirror wgpu’s enabled set.** If we tell rive `fragmentShaderPixelInterlock=true` but wgpu didn’t actually enable the ext/feature (e.g. the `open_with_callback` chain didn’t take), rive emits pipelines the device rejects. Mitigation: probe device extensions before `open_with_callback`; after device creation, read back the merged `pEnabledFeatures`/enabled extensions from the hal adapter and set the rive booleans from the OBSERVED-enabled set, not the requested set. Also confirm `fragmentStoresAndAtomics` is actually enabled (rive REQUIRES it) — wgpu enables it via downlevel flags but verify.

5. **(Low — mostly resolved by f8/f6/f7) wgpu/wgpu-hal/bevy 27 call shapes.** The previously-uncertain shapes are now confirmed verbatim: `open_with_callback` takes `Box<dyn FnOnce(CreateDeviceCallbackArgs)>` with `args.extensions`/`args.device_features`/`args.create_info` (f8); `as_hal` is the GUARD form for Device/Adapter/Texture (f8); `Device::as_hal` exposes `queue_family_index()`+`raw_queue()` (f8); `Node2d::MainTransparentPass` is the sprite pass and `Node2d::StartMainPass` is the safe pre-sampling edge (f7); `RenderResources`/`RenderAdapterInfo` tuples (f6). Remaining impl-time checks: the ash 0.38 builder method names for the EXT feature structs, and `RenderAdapter::get_info()`/`AdapterInfo` ctor for `RenderAdapterInfo` if using Path B. None change the architecture.

5b. **(Medium) `raw_vulkan_init` feature must be enabled + propagated.** Path A requires `bevy_render/raw_vulkan_init` (= `wgpu/vulkan`) on the bevy dep; it is NON-default and cfg-gates a 6th `AdditionalVulkanFeatures` field on `RenderResources`/`RenderCreation::manual`. The `add_create_device_callback` registrar is `unsafe` and contract-bound (must not remove features / request unsupported) — probe the physical device first. If the feature can't be enabled in the consumer's bevy build, fall back to Path B (which doesn't need it, but then constructs the cfg-gated `RenderResources` and so ALSO needs the feature to add the 6th field) — net: M1b's zero-copy tier requires `raw_vulkan_init` either way; without it, only the M1a CPU floor is available. (Flag for the Cargo.toml.)

6. **(Medium) `safeFrameNumber` correctness under future pipelining.** This milestone waits the fence every frame, so the watermark is trivially correct (`safe=N-1`). When M2 removes the per-frame wait for pipelining, `safeFrameNumber` MUST be driven from observed fence completion (3-deep, `VulkanContext::MaxFramesInFlight`), or rive recycles in-flight pooled buffers → corruption (f0).

7. **(Medium — CORRECTED, and tightened by M4/M5 §10.0/§3.1/§3.2) Render-world state residency: Send+Sync wrapper, NOT `NonSend`; refcount must be `Arc`, not `Rc`.** f7/f_extract found that `PipelinedRenderingPlugin` runs the render world on a spawned OS thread and render-world `NonSend` data is thread-affine/fragile there, so rive’s `!Send` objects live in a normal `Resource` wrapping the FFI pointers in a newtype with hand-written `unsafe impl Send + Sync` + a single-thread invariant (created/owned in the render world; Extract carries only `Send` data). **M4 caveat (applied):** `unsafe impl Send+Sync` over an `Rc<ContextInner>` is still UNSOUND because the SubApp ferry-for-drop (pipelined_rendering.rs:56-66) can drop a render-thread-created resource on the main thread → cross-thread non-atomic refcount = UB. The M1b path uses **`Arc`** (atomic) for the rive graph (or raw pointers freed in the resource's `Drop`). **M5 caveat (applied):** `Node::run` has only `&World`, so the `&mut` `advance`/`draw` go through a `RefCell<RiveFrameState>` inside `RiveGpu`, with `Node::run` the documented sole per-frame mutator (that `RefCell` is `!Sync`, which is why the `unsafe impl Sync` is needed). (Alternatively, disable `PipelinedRenderingPlugin` and use `Rc`+`NonSend` — but the `Arc`+wrapper resource is the chosen path.)

8. **(Low) WSL2 has no interlock.** M1b falls to `atomics`/offscreen-copy on Dozen. Default WSL2 to `RIVE_TIER=cpu`; native validation is on real hardware (Linux interlock / Windows 4090 relay).

9. **(Low) Display double-texture.** Option B keeps an internal `Rgba8Unorm` shared texture + an `Rgba8UnormSrgb` display texture. Two GPU textures per target. Accepted for seam uniformity; Option A (single texture, custom material) is the documented alternative if memory matters.

10. **(Medium — NEEDS USER SIGN-OFF, M6) Pixel parity vs frozen seam.** This spec selects **visual-equivalence (≤2 LSB on AA edges, opaque exact)** to preserve the unchanged `Rgba8UnormSrgb` Sprite seam (§7.3). Byte-exact M1a reproduction is incompatible with the frozen seam (it needs an `Rgba8Unorm` non-decoding display path). If the locked constraint truly demands byte-exact, the user must approve breaking the Sprite seam. Flagged for sign-off.

11. **(Low — accepted, N7) Animation clock.** The extracted `RiveTarget` payload advances rive by **main-world** `dt*speed` (f_extract), not render-thread cadence. With per-frame fence-wait stalls the render cadence and main-world `Time` can diverge; driving animation speed by main-world dt is correct. Documented so it is not mistaken for a bug.

12. **(Low — N1) ABI is 64-bit-only.** All Vulkan handles cross the FFI as `uint64_t`; dispatchable handles (`VkQueue`/`VkDevice`/`VkInstance`) are pointers reinterpreted as `u64` and would truncate on a hypothetical 32-bit target. Documented assumption (§2.3 ABI contract).

13. **(Low — N3) Teardown order.** In `rive_render_context_destroy` (external), destroy `extPool` via `vulkanContext()`'s dispatch table FIRST, THEN `reset(renderContext)` — resetting the context drops the `VulkanContext`/dispatch table, so the reverse order is a use-after-free (§2.4).

---

## EXECUTIVE SUMMARY (6–10 lines)

M1b adds a zero-copy Vulkan tier alongside the unchanged M1a CPU-copy floor; tier selection is internal to `RivePlugin` and the frozen ECS API (`RiveAnimation`/`RiveTarget`/`RiveFile` + the `Handle<Image>`/upright seam) is untouched. The device is shared via Bevy’s first-class `raw_vulkan_init` hook (PRIMARY): a `RawVulkanInitSettings` device callback appends `VK_EXT_fragment_shader_interlock` (or `…rasterization_order_attachment_access`) + the chained feature struct inside Bevy’s own `open_with_callback`, so Bevy keeps owning the wgpu device; the `RenderCreation::Manual` hand-built device is the documented fallback. We extract the raw `VkInstance/VkPhysicalDevice/VkDevice/VkQueue/familyIndex/loader` from Bevy’s `RenderDevice`/`RenderAdapter`/`RenderInstance` via the guard-form `as_hal` (NOT closures), mirroring the actually-enabled extensions into rive’s `VulkanFeatures`. The shim gains `rive_render_context_create_vulkan_external` (→ `MakeContext`), `rive_render_target_wrap_vk_image` (→ `makeRenderTarget`+`setTargetImageView`/`makeExternalImageView`), `rive_render_context_pls_mode` (→ `frameInterlockMode`), and an out-of-band frame path that allocates rive’s cb from a caller-provided pool, records rive’s flush + layout barriers into it, and submits to the wgpu queue with a caller fence — rive never submits. rive’s `!Send` state lives in the **render world as a Send+Sync wrapper resource** (NOT `NonSend`, which is fragile under `PipelinedRenderingPlugin`), created/touched only on the render thread; a stateless `RiveFillNode` (ordered before `Node2d::StartMainPass`) advances+draws+submits+fences, then an Option-B fullscreen un-premultiply+sRGB-decode pass writes a straight-alpha `Rgba8UnormSrgb` texture displayed by the unchanged M1a Sprite — a uniform display seam across both tiers, with `Rgba8Unorm` (not `*UnormSrgb`) as the shared texture so the WGSL sees raw encoded premultiplied bytes. d3d12/metal siblings are declared (stubbed). Validation is native-only (Linux interlock + Windows `WGPU_BACKEND=vulkan` relay); transparency is exercised by a transparent clear over a colored backdrop since no transparent `.riv` exists.

**Riskiest 3 points:**
1. **Image-layout reconciliation** between rive’s own barriers and wgpu’s independent layout tracker on the shared VkImage — must validate with Vulkan validation layers on native; clean `transition_resources` ownership is deferred to M2.
2. **Out-of-band `vkQueueSubmit` on wgpu’s shared, non-thread-safe queue** — issued while holding the wgpu queue guard (`Queue::as_hal`, M2) before the sampling pass; whether that guard serializes wgpu's internal submit is unverified, so re-verify wgpu 27's queue-lock model and keep the same-render-thread/drain fallback. The completion barrier is the MANDATORY per-frame `VkFence` wait (NOT `device.poll`, which can't sync an out-of-band submit — B1/B2).
3. **Zero-copy degrades to rive’s internal offscreen-copy for blended content** because attachment-only usage isn't a valid PLS color attachment; the copy-back requires `TRANSFER_DST`, so the shared image MUST be `RENDER_ATTACHMENT|TEXTURE_BINDING|COPY_DST|COPY_SRC` (M3 — without it, blended content is blank/garbage, NOT "still correct"). This is correct-but-copying for the blend path; the pure-`INPUT_ATTACHMENT`-via-`create_texture_from_hal` zero-copy variant is the documented M2 stub — plus the standing requirement that the rive `VulkanFeatures` exactly mirror wgpu’s actually-enabled device features or rive emits pipelines the device rejects.
