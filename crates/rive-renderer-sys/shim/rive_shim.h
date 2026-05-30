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

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* RIVE_SHIM_H */
