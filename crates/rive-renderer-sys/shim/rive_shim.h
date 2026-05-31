/*
 * rive_shim.h — minimal C ABI over the native Rive Renderer (rive-runtime PLS,
 * Vulkan backend), for Milestone 0: render a .riv offscreen and read pixels back.
 *
 * This ABI is adapted from the project's original RiveSharp-style sketch to the
 * REAL rive-runtime API. Notable, deliberate deviations from the sketch (each is
 * a consequence of the real source; see BUILD.md "C ABI deviations"):
 *
 *   - The context is created by `rive_render_context_create_vulkan_self`, which
 *     uses rive's own `rive_vk_bootstrap` (compiled INTO this shim) to create a
 *     headless VkInstance/VkPhysicalDevice/VkDevice + graphics queue, then calls
 *     `rive::gpu::RenderContextVulkanImpl::MakeContext(...)`.
 *   - An offscreen `RiveRenderTarget` bundles rive's `RenderTargetVulkanImpl`
 *     with a `rive_vkb::VulkanHeadlessFrameSynchronizer` (the offscreen image,
 *     per-frame command buffer, fence, and CPU readback all live there).
 *   - `rive_file_load` imports via `rive::File::import`, passing the RenderContext
 *     itself AS the `rive::Factory` (RenderContext IS-A Factory).
 *   - "state machine" is backed by a `rive::Scene` (default state machine if the
 *     designer set one, else `defaultScene()`); `advance` calls advanceAndApply.
 *   - `rive_frame_begin/_draw/_flush` map onto beginFrame / RiveRenderer +
 *     artboard->draw / flush(FlushResources) + queueImageCopy + endFrame +
 *     getPixelsFromLastImageCopy. Drawing fits the artboard with Fit::contain +
 *     Alignment::center.
 *   - Pixel readback yields PREMULTIPLIED, top-down, sRGB-encoded RGBA8
 *     (VK_FORMAT_R8G8B8A8_UNORM). The caller un-premultiplies for a viewer.
 *
 * Error model: constructors return NULL on failure; verbs return a `RiveStatus`
 * (0 == success). No C++ exceptions cross this boundary (rive is built with
 * exceptions off, and the shim catches at the boundary). `rive_last_error`
 * returns a human-readable description of the most recent failure (M0: a single
 * global, not thread-safe).
 */
#ifndef RIVE_SHIM_H
#define RIVE_SHIM_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct RiveRenderContext RiveRenderContext;
typedef struct RiveRenderTarget  RiveRenderTarget;
typedef struct RiveFile          RiveFile;
typedef struct RiveArtboard      RiveArtboard;
typedef struct RiveStateMachine  RiveStateMachine;

/* 0 == success; nonzero == failure (see rive_last_error). */
typedef int32_t RiveStatus;
#define RIVE_OK 0

/* Returns a static, human-readable description of the most recent failure, or
 * an empty string if none. Valid until the next failing shim call. */
const char* rive_last_error(void);

/* --- Context (M0: shim creates and owns its own VkInstance/VkDevice) ------- */

/* Creates a headless Vulkan device and a native Rive RenderContext on it.
 * Honors env vars: RIVE_GPU (substring GPU-name filter; "integrated" picks an
 * integrated GPU) and RIVE_FORCE_ATOMIC (if set, forces the atomic PLS path).
 * Returns NULL on failure. */
RiveRenderContext* rive_render_context_create_vulkan_self(void);
void               rive_render_context_destroy(RiveRenderContext*);

/* --- Offscreen render target (rive render target + headless synchronizer) -- */

RiveRenderTarget*  rive_render_target_create_offscreen(RiveRenderContext*,
                                                       uint32_t width,
                                                       uint32_t height);
void               rive_render_target_destroy(RiveRenderTarget*);
uint32_t           rive_render_target_width(const RiveRenderTarget*);
uint32_t           rive_render_target_height(const RiveRenderTarget*);
/* Size in bytes of the RGBA8 readback buffer == width * height * 4. */
size_t             rive_render_target_pixel_buffer_size(const RiveRenderTarget*);

/* --- File / artboard / state machine --------------------------------------- */

/* Imports a .riv from memory using `ctx` as the rive::Factory. The bytes are
 * only borrowed for the duration of the call. Returns NULL on failure. */
RiveFile*          rive_file_load(RiveRenderContext* ctx,
                                  const uint8_t* bytes,
                                  size_t len);
void               rive_file_destroy(RiveFile*);

/* Instantiates the file's default artboard. Returns NULL if the file has none. */
RiveArtboard*      rive_file_artboard_default(RiveFile*);
void               rive_artboard_destroy(RiveArtboard*);

/* Instantiates the artboard's default state machine, falling back to its
 * default Scene (first state machine, else first animation, else static).
 * Returns NULL if nothing is playable. */
RiveStateMachine*  rive_artboard_state_machine_default(RiveArtboard*);
void               rive_state_machine_destroy(RiveStateMachine*);

/* Advances the state machine (advanceAndApply) by `dt_seconds`, applying the
 * result to its backing artboard. */
void               rive_state_machine_advance(RiveStateMachine*, float dt_seconds);

/* --- Frame: begin -> draw -> flush ----------------------------------------- */

/* Begins a frame against `target`, clearing to the given straight (non-
 * premultiplied) RGBA color in [0, 1]. Exactly one frame may be in flight per
 * context. */
RiveStatus         rive_frame_begin(RiveRenderContext* ctx,
                                    RiveRenderTarget* target,
                                    float r, float g, float b, float a);

/* Draws `artboard` into the current frame, fit with Fit::contain +
 * Alignment::center to the target. Call after advancing. */
RiveStatus         rive_artboard_draw(RiveArtboard* artboard,
                                      RiveRenderContext* ctx);

/* Submits the frame, copies the result back to a CPU buffer held by the target,
 * and waits for the GPU. After this, use rive_render_target_read_pixels. */
RiveStatus         rive_frame_flush(RiveRenderContext* ctx);

/* --- Readback (M0 validation) ---------------------------------------------- */

/* Copies the most recently flushed frame's pixels into `out_rgba`.
 * `out_len` must equal rive_render_target_pixel_buffer_size(). Pixels are
 * RGBA8, top-down, sRGB-encoded, with PREMULTIPLIED alpha. */
RiveStatus         rive_render_target_read_pixels(RiveRenderTarget* target,
                                                  uint8_t* out_rgba,
                                                  size_t out_len);

/* ====================================================================== *
 * M1b: external (wgpu-shared) Vulkan tier — ZERO-COPY shared VkImage.
 *
 * In M1b, wgpu owns the VkInstance/VkPhysicalDevice/VkDevice/VkQueue; the shim
 * BORROWS them (never creates/destroys them) and renders the .riv directly into
 * a wgpu-allocated VkImage. rive's flush RECORDS into a command buffer the shim
 * allocates from its own per-frame pool; the shim then submits OUT-OF-BAND to
 * the wgpu graphics queue with a caller-owned VkFence. rive itself never
 * submits. The caller (Rust/Bevy) owns the pool family, the queue, and the
 * fence lifecycle, and waits the fence before the sampling pass.
 *
 * All Vulkan handles cross this ABI as `uint64_t` (the integer value of the
 * dispatchable/non-dispatchable handle, as exposed by wgpu-hal/ash), so the
 * ABI itself carries no Vulkan headers. 64-bit hosts only (dispatchable handles
 * are pointers).
 * ====================================================================== */

/* rive's PLS interlock mode (gpu::InterlockMode ordinals; pinned by
 * static_assert in the .cpp). -1 == null handle / not currently in a frame. */
typedef int32_t RivePlsMode;
#define RIVE_PLS_RASTER_ORDERING  0
#define RIVE_PLS_ATOMICS          1
#define RIVE_PLS_CLOCKWISE        2
#define RIVE_PLS_CLOCKWISE_ATOMIC 3
#define RIVE_PLS_MSAA             4

/* Mirror of rive::gpu::VulkanFeatures (vulkan_context.hpp). The caller fills
 * this from what wgpu ACTUALLY enabled on the shared VkDevice. C-stable layout;
 * the shim copies field-by-field into rive's struct (never reinterpret-casts).
 * Bools are int32 (0/nonzero) for a stable ABI. */
typedef struct RiveVulkanFeatures {
    uint32_t apiVersion;                              /* e.g. VK_API_VERSION_1_1 (0x00401000) */
    int32_t  independentBlend;
    int32_t  fillModeNonSolid;
    int32_t  fragmentStoresAndAtomics;               /* REQUIRED for core operation (atomic fallback) */
    int32_t  shaderClipDistance;
    int32_t  rasterizationOrderColorAttachmentAccess;/* EXT_rasterization_order_attachment_access */
    int32_t  fragmentShaderPixelInterlock;           /* VK_EXT_fragment_shader_interlock */
    int32_t  vkKhrPortabilitySubset;
    int32_t  textureCompressionBC;
    int32_t  textureCompressionASTC_LDR;
    int32_t  textureCompressionETC2;
} RiveVulkanFeatures;

/* Create a rive RenderContext on a wgpu-OWNED Vulkan device. The shim does NOT
 * create or destroy the instance/device — it only borrows them.
 *
 *   instance/physicalDevice/device : the wgpu-owned VkInstance/VkPhysicalDevice/VkDevice
 *   getInstanceProcAddr            : PFN_vkGetInstanceProcAddr (a raw fn pointer value)
 *   features                       : MUST mirror exactly what wgpu enabled on `device`
 *   forceAtomic                    : if nonzero, ContextOptions.forceAtomicMode = true
 *
 * Returns NULL on failure. Destroy with rive_render_context_destroy (which, for
 * an external context, resets only the RenderContext and never touches the
 * device/instance). */
RiveRenderContext* rive_render_context_create_vulkan_external(
    uint64_t instance,
    uint64_t physicalDevice,
    uint64_t device,
    void*    getInstanceProcAddr,            /* PFN_vkGetInstanceProcAddr */
    const RiveVulkanFeatures* features,
    int32_t  forceAtomic);

/* The graphics queue-family index the shim allocates its per-frame command pool
 * on. Call ONCE after creating an external context, before the first frame.
 * (Stored on the context; the pool is created lazily on first submit.) */
void rive_render_context_set_queue_family(RiveRenderContext* ctx,
                                          uint32_t queueFamilyIndex);

/* M2.0 perf lever: per-frame `clockwiseFillOverride` (rive FrameDescriptor). When
 * nonzero, rive's select_interlock_mode prefers its clockwise PLS path (clockwise
 * if the device supports it, else clockwiseAtomic) over atomics — relevant on
 * desktop NVIDIA, which lacks the raster-order ext so its default path is atomics.
 * Off by default; set once after create. Honored by rive_frame_begin_external. */
void rive_render_context_set_clockwise(RiveRenderContext* ctx, int32_t enabled);

/* Frame-independent: does the shared VkDevice give rive its clean raster-order
 * PLS path? 1 == yes, 0 == no (atomic/msaa fallback), -1 == null handle. Valid
 * any time after create. */
int32_t rive_render_context_supports_raster_ordering(const RiveRenderContext* ctx);

/* Active per-frame interlock mode (gpu::InterlockMode ordinal; see RIVE_PLS_*).
 * Valid ONLY between rive_frame_begin_external and rive_frame_submit_external.
 * -1 on null. */
RivePlsMode rive_render_context_pls_mode(const RiveRenderContext* ctx);

/* M2.0: GPU execution time (milliseconds) of the most recent external frame's
 * rive command buffer, measured with VkQueryPool timestamps written around rive's
 * recorded work (begin -> flush -> post-flush barrier). The blocking submit
 * guarantees the result is ready on return. Returns -1.0 if GPU timing is
 * unavailable (device lacks reliable timestamps, or the timestamp PFNs/pool could
 * not be set up). */
double rive_render_context_last_gpu_ms(const RiveRenderContext* ctx);

/* M2a: CPU-side sub-span timings of the most recent external frame, microseconds,
 * for the fence-vs-flush perf split (Step 0). `flush_us` is rive's CPU-side
 * RenderContext::flush() (command-buffer record + rive's own CPU work); the
 * `fence_wait_us` is the blocking vkWaitForFences after the out-of-band submit
 * (the cost the M2a non-blocking-sync rework targets). -1.0 if no external frame
 * has run yet. The remainder of render_external_frame's wall (begin/end CB, the
 * post-flush barrier, ResetFences, QueueSubmit, timestamp readback) is "other" =
 * total - flush - fence_wait. */
double rive_render_context_last_flush_us(const RiveRenderContext* ctx);
double rive_render_context_last_fence_wait_us(const RiveRenderContext* ctx);

/* Wrap a wgpu-ALLOCATED VkImage as a rive render target (ZERO COPY). The shim
 * does NOT allocate or free the image — wgpu owns it. If `vkImageView` is 0 the
 * shim creates a matching view (via makeExternalImageView) and owns THAT view
 * only. `vkFormat` is the wgpu texture's VkFormat (Rgba8Unorm == 37 ==
 * VK_FORMAT_R8G8B8A8_UNORM); `vkUsageFlags` is the VkImageUsageFlags wgpu
 * created it with (must include INPUT_ATTACHMENT or both TRANSFER_SRC+DST per
 * rive's render-target contract — the Rust side allocates
 * RENDER_ATTACHMENT|TEXTURE_BINDING|COPY_DST|COPY_SRC).
 *
 * Returns NULL on failure. Destroy with rive_render_target_destroy (which, for
 * an external target, drops the rive wrapper + any shim-created view, and never
 * frees the wgpu image). */
RiveRenderTarget* rive_render_target_wrap_vk_image(
    RiveRenderContext* ctx,
    uint64_t vkImage,
    uint64_t vkImageView,
    uint32_t width,
    uint32_t height,
    uint32_t vkFormat,
    uint32_t vkUsageFlags);

/* Rebind the wgpu VkImage/view on an existing external target (e.g. after the
 * GpuImage was reprepared/resized). Pass vkImageView=0 to have the shim
 * recreate the view. Resets the tracked layout to UNDEFINED. */
void rive_render_target_set_vk_image(RiveRenderTarget* target,
                                     uint64_t vkImage,
                                     uint64_t vkImageView);

/* Begin a frame against a wrapped external target. Like rive_frame_begin but
 * with no synchronizer; the caller supplies the frame-number watermark:
 *   currentFrameNumber : monotonically increasing, MUST be nonzero
 *   safeFrameNumber    : highest frame the caller has OBSERVED the GPU finished
 * Clear color is straight (non-premultiplied) RGBA in [0,1]. */
RiveStatus rive_frame_begin_external(RiveRenderContext* ctx,
                                     RiveRenderTarget* target,
                                     float r, float g, float b, float a,
                                     uint64_t currentFrameNumber,
                                     uint64_t safeFrameNumber);

/* (Draw with rive_artboard_draw — it is REUSED verbatim for both tiers.) */

/* Record rive's draws + the post-flush COLOR->SHADER_READ_ONLY barrier into a
 * command buffer the shim allocates from its per-frame pool (on the queue family
 * set above), then vkEndCommandBuffer + vkQueueSubmit OUT-OF-BAND to `queue`
 * with a shim-internal fence, then BLOCK on that fence. rive RECORDS; the shim
 * owns begin/end/submit/wait. NO readback, NO pixel flip. On return the shared
 * image is fully written and left in SHADER_READ_ONLY_OPTIMAL, ready to sample.
 *
 *   queue : the wgpu graphics VkQueue (the Rust side serializes against wgpu's
 *           queue use; see the M1b report)
 *
 * The fence is internal (the Rust side cannot cheaply build an ash::Device to
 * make one). M1b is correctness-first: this call is BLOCKING. Splitting submit
 * from wait for pipelining is M2. */
RiveStatus rive_frame_submit_external(RiveRenderContext* ctx,
                                      RiveRenderTarget* target,
                                      uint64_t queue);

/* M2a NON-BLOCKING path. Like rive_frame_submit_external, but RECORDS rive's draws
 * + the ->SHADER_READ_ONLY barrier into `cmdBuffer` — the CALLER's already-open
 * command buffer (wgpu's own, from as_hal_mut().raw_handle()) — and returns WITHOUT
 * begin/end/submit/fence. rive's work rides wgpu's single per-frame submit,
 * GPU-ordered before the wgpu pass that samples the image; no CPU stall.
 *
 *   cmdBuffer : wgpu's open primary VkCommandBuffer for this frame (u64 handle).
 *
 * The caller must seed safeFrameNumber (at begin) to trail currentFrameNumber by
 * rive's ring size (no fence → a frame is recyclable only once its GPU work has
 * completed, bounded by frames-in-flight). On return the image is left in
 * SHADER_READ_ONLY_OPTIMAL == wgpu's tracked RESOURCE layout. */
RiveStatus rive_frame_record_external(RiveRenderContext* ctx,
                                      RiveRenderTarget* target,
                                      uint64_t cmdBuffer);

/* The VkImage / VkImageView the external target currently points at (0 if not
 * external). Diagnostics. */
uint64_t rive_render_target_vk_image(const RiveRenderTarget* target);
uint64_t rive_render_target_vk_image_view(const RiveRenderTarget* target);

/* ---- Backend-tagged d3d12 / metal siblings (DESIGN ONLY; stubbed in M1b) ----
 *
 * Declared so the cross-backend ABI shape is uniform and M2/M3 can implement
 * them without ABI churn. In this build they set rive_last_error and return
 * NULL / nonzero. The signatures encode each backend's submission model:
 *   - Vulkan : VkCommandBuffer recorded by rive, submitted by us to a VkQueue
 *              with a VkFence (above).
 *   - D3D12  : rive records into its own command list; the caller drives an
 *              ID3D12CommandQueue + ID3D12Fence/value (no external cmd buffer).
 *   - Metal  : rive's FlushResources.externalCommandBuffer is id<MTLCommandBuffer>,
 *              which self-submits via `commit`.
 */
RiveRenderContext* rive_render_context_create_d3d12_external(
    void* d3d12Device, void* d3d12CommandQueue, int32_t forceAtomic);
RiveRenderTarget*  rive_render_target_wrap_d3d12_resource(
    RiveRenderContext* ctx, void* d3d12Resource,
    uint32_t width, uint32_t height, uint32_t dxgiFormat);
RiveStatus         rive_frame_submit_external_d3d12(
    RiveRenderContext* ctx, RiveRenderTarget* target,
    void* d3d12CommandQueue, void* d3d12Fence, uint64_t fenceValue);

RiveRenderContext* rive_render_context_create_metal_external(
    void* mtlDevice, void* mtlCommandQueue);
RiveRenderTarget*  rive_render_target_wrap_metal_texture(
    RiveRenderContext* ctx, void* mtlTexture,
    uint32_t width, uint32_t height, uint32_t mtlPixelFormat);
RiveStatus         rive_frame_submit_external_metal(
    RiveRenderContext* ctx, RiveRenderTarget* target,
    void* mtlCommandBuffer /* caller-owned id<MTLCommandBuffer> */);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* RIVE_SHIM_H */
