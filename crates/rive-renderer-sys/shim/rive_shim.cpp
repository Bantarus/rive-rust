/*
 * rive_shim.cpp — implementation of the M0 C ABI (see rive_shim.h).
 *
 * Bridges a flat C ABI to the real rive-runtime C++ API and rive's own
 * `rive_vk_bootstrap` helpers (whose .cpp sources are compiled into this shim,
 * since rive-runtime does not build them into a static lib).
 */

#include "rive_shim.h"

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <new>
#include <string>
#include <vector>

#include <vulkan/vulkan.h>

// Core scene graph.
#include "rive/file.hpp"
#include "rive/artboard.hpp"
#include "rive/scene.hpp"
#include "rive/animation/state_machine_instance.hpp"
#include "rive/factory.hpp"
#include "rive/layout.hpp"             // Fit, Alignment
#include "rive/math/aabb.hpp"
#include "rive/math/mat2d.hpp"
#include "rive/refcnt.hpp"             // rcp, ref_rcp
#include "rive/renderer.hpp"           // Renderer, computeAlignment
#include "rive/shapes/paint/color.hpp" // colorARGB, ColorInt
#include "rive/span.hpp"

// PLS renderer + Vulkan backend.
#include "rive/renderer/render_context.hpp"
#include "rive/renderer/rive_renderer.hpp"
#include "rive/renderer/vulkan/render_context_vulkan_impl.hpp"
#include "rive/renderer/vulkan/render_target_vulkan.hpp"
#include "rive/renderer/vulkan/vulkan_context.hpp"

// rive's headless Vulkan bootstrap (sources compiled into this shim). NOTE: the
// rive_vk_bootstrap headers have NO include guards, and the headless header
// already includes vulkan_frame_synchronizer.hpp — so we must NOT include the
// latter directly here, or VulkanFrameSynchronizer is defined twice.
#include "rive_vk_bootstrap/vulkan_instance.hpp"
#include "rive_vk_bootstrap/vulkan_device.hpp"
#include "rive_vk_bootstrap/vulkan_headless_frame_synchronizer.hpp"

using rive::gpu::RenderContext;
using rive::gpu::RenderContextVulkanImpl;
using rive::gpu::RenderTargetVulkanImpl;
using rive::gpu::VulkanContext;
using rive::gpu::vkutil::ImageAccess;

namespace {

// M0-only, single-threaded last-error storage.
std::string& last_error_storage()
{
    static std::string s;
    return s;
}
void set_error(const char* msg) { last_error_storage() = msg ? msg : ""; }

bool debug_enabled() { return std::getenv("RIVE_DEBUG") != nullptr; }

// Flips an RGBA8 buffer vertically, in place (row 0 <-> row height-1, ...).
void flip_rows_vertically(std::vector<uint8_t>& pixels,
                          uint32_t width,
                          uint32_t height)
{
    const size_t rowBytes = static_cast<size_t>(width) * 4u;
    if (rowBytes == 0 || height < 2 || pixels.size() != rowBytes * height)
    {
        return;
    }
    std::vector<uint8_t> tmp(rowBytes);
    for (uint32_t y = 0; y < height / 2; ++y)
    {
        uint8_t* top = pixels.data() + static_cast<size_t>(y) * rowBytes;
        uint8_t* bottom =
            pixels.data() + static_cast<size_t>(height - 1 - y) * rowBytes;
        std::memcpy(tmp.data(), top, rowBytes);
        std::memcpy(top, bottom, rowBytes);
        std::memcpy(bottom, tmp.data(), rowBytes);
    }
}

uint8_t to_u8(float v)
{
    if (v <= 0.0f)
        return 0;
    if (v >= 1.0f)
        return 255;
    return static_cast<uint8_t>(v * 255.0f + 0.5f);
}

} // namespace

// ---------------------------------------------------------------------------
// Opaque handle definitions.
// ---------------------------------------------------------------------------

struct RiveRenderContext
{
    std::unique_ptr<rive_vkb::VulkanInstance> instance;
    std::unique_ptr<rive_vkb::VulkanDevice> device;
    std::unique_ptr<RenderContext> renderContext;
    RenderContextVulkanImpl* impl = nullptr; // borrowed (owned by renderContext)

    // One in-flight frame at a time (M0).
    rive::RiveRenderer* currentRenderer = nullptr; // owned between begin/flush
    RiveRenderTarget* currentTarget = nullptr;     // borrowed
};

struct RiveRenderTarget
{
    std::unique_ptr<rive_vkb::VulkanHeadlessFrameSynchronizer> sync;
    rive::rcp<RenderTargetVulkanImpl> renderTarget;
    uint32_t width = 0;
    uint32_t height = 0;
    std::vector<uint8_t> pixels; // last flushed frame, RGBA8 premultiplied top-down
};

struct RiveFile
{
    rive::rcp<rive::File> file;
};

struct RiveArtboard
{
    std::unique_ptr<rive::ArtboardInstance> artboard;
};

struct RiveStateMachine
{
    std::unique_ptr<rive::Scene> scene;
};

// ---------------------------------------------------------------------------
// Error string.
// ---------------------------------------------------------------------------

extern "C" const char* rive_last_error(void) { return last_error_storage().c_str(); }

// ---------------------------------------------------------------------------
// Context.
// ---------------------------------------------------------------------------

extern "C" RiveRenderContext* rive_render_context_create_vulkan_self(void)
{
    auto ctx = new (std::nothrow) RiveRenderContext();
    if (ctx == nullptr)
    {
        set_error("out of memory allocating RiveRenderContext");
        return nullptr;
    }

    // 1. Headless Vulkan instance (no surface extensions). Validation layers are
    // not required for M0 and are often absent, so disable them by default to
    // avoid a noisy "validation layers not supported" warning.
    rive_vkb::VulkanInstance::Options instanceOpts;
    instanceOpts.appName = "rive-renderer-sys (M0 offscreen)";
    instanceOpts.desiredValidationType = rive_vkb::VulkanValidationType::none;
    instanceOpts.wantDebugCallbacks = false;
    ctx->instance = rive_vkb::VulkanInstance::Create(instanceOpts);
    if (ctx->instance == nullptr)
    {
        set_error("rive_vkb::VulkanInstance::Create failed (is libvulkan.so.1 "
                  "installed?)");
        delete ctx;
        return nullptr;
    }

    // 2. Headless device (picks discrete GPU first; honors RIVE_GPU env).
    rive_vkb::VulkanDevice::Options deviceOpts;
    deviceOpts.headless = true;
    // Quiet by default; set RIVE_DEBUG=1 to print the selected GPU + features.
    deviceOpts.printInitializationMessage = debug_enabled();
    if (const char* gpu = std::getenv("RIVE_GPU"))
    {
        deviceOpts.gpuNameFilter = gpu;
    }
    ctx->device = rive_vkb::VulkanDevice::Create(*ctx->instance, deviceOpts);
    if (ctx->device == nullptr)
    {
        set_error("rive_vkb::VulkanDevice::Create failed (no compatible Vulkan "
                  "device?)");
        delete ctx;
        return nullptr;
    }

    // 3. Native Rive render context on the borrowed device handles.
    RenderContextVulkanImpl::ContextOptions ctxOpts;
    ctxOpts.forceAtomicMode = std::getenv("RIVE_FORCE_ATOMIC") != nullptr;
    ctx->renderContext = RenderContextVulkanImpl::MakeContext(
        ctx->instance->vkInstance(),
        ctx->device->vkPhysicalDevice(),
        ctx->device->vkDevice(),
        ctx->device->vulkanFeatures(),
        ctx->instance->getVkGetInstanceProcAddrPtr(),
        ctxOpts);
    if (ctx->renderContext == nullptr)
    {
        set_error("RenderContextVulkanImpl::MakeContext failed");
        delete ctx;
        return nullptr;
    }
    ctx->impl = ctx->renderContext->static_impl_cast<RenderContextVulkanImpl>();
    return ctx;
}

extern "C" void rive_render_context_destroy(RiveRenderContext* ctx)
{
    if (ctx == nullptr)
        return;
    // A frame may have been begun but never flushed; clean up its renderer.
    delete ctx->currentRenderer;
    ctx->currentRenderer = nullptr;
    // Destruction order: render context (drops its VulkanContext ref), then
    // device, then instance. Any RiveRenderTarget must already be destroyed.
    ctx->renderContext.reset();
    ctx->device.reset();
    ctx->instance.reset();
    delete ctx;
}

// ---------------------------------------------------------------------------
// Offscreen render target.
// ---------------------------------------------------------------------------

extern "C" RiveRenderTarget* rive_render_target_create_offscreen(
    RiveRenderContext* ctx,
    uint32_t width,
    uint32_t height)
{
    if (ctx == nullptr || ctx->impl == nullptr || width == 0 || height == 0)
    {
        set_error("rive_render_target_create_offscreen: invalid arguments");
        return nullptr;
    }

    auto target = new (std::nothrow) RiveRenderTarget();
    if (target == nullptr)
    {
        set_error("out of memory allocating RiveRenderTarget");
        return nullptr;
    }
    target->width = width;
    target->height = height;

    // R8G8B8A8_UNORM keeps the readback bytes sRGB-encoded with no hardware
    // gamma conversion; TRANSFER_SRC is required for the copy-to-buffer readback.
    const VkFormat format = VK_FORMAT_R8G8B8A8_UNORM;
    const VkImageUsageFlags usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT |
                                    VK_IMAGE_USAGE_TRANSFER_SRC_BIT |
                                    VK_IMAGE_USAGE_TRANSFER_DST_BIT;

    rive_vkb::VulkanHeadlessFrameSynchronizer::Options syncOpts;
    syncOpts.width = width;
    syncOpts.height = height;
    syncOpts.imageFormat = format;
    syncOpts.imageUsageFlags = usage;
    target->sync = rive_vkb::VulkanHeadlessFrameSynchronizer::Create(
        *ctx->instance,
        *ctx->device,
        rive::ref_rcp(ctx->impl->vulkanContext()),
        syncOpts);
    if (target->sync == nullptr)
    {
        set_error("VulkanHeadlessFrameSynchronizer::Create failed");
        delete target;
        return nullptr;
    }

    target->renderTarget = ctx->impl->makeRenderTarget(width, height, format, usage);
    if (target->renderTarget == nullptr)
    {
        set_error("RenderContextVulkanImpl::makeRenderTarget failed");
        delete target;
        return nullptr;
    }
    return target;
}

extern "C" void rive_render_target_destroy(RiveRenderTarget* target)
{
    if (target == nullptr)
        return;
    // Destroy the synchronizer first (it waits for in-flight command buffers),
    // then drop the render target wrapper.
    target->sync.reset();
    target->renderTarget = nullptr;
    delete target;
}

extern "C" uint32_t rive_render_target_width(const RiveRenderTarget* target)
{
    return target != nullptr ? target->width : 0;
}

extern "C" uint32_t rive_render_target_height(const RiveRenderTarget* target)
{
    return target != nullptr ? target->height : 0;
}

extern "C" size_t rive_render_target_pixel_buffer_size(const RiveRenderTarget* target)
{
    if (target == nullptr)
        return 0;
    return static_cast<size_t>(target->width) * target->height * 4u;
}

// ---------------------------------------------------------------------------
// File / artboard / state machine.
// ---------------------------------------------------------------------------

extern "C" RiveFile* rive_file_load(RiveRenderContext* ctx,
                                    const uint8_t* bytes,
                                    size_t len)
{
    if (ctx == nullptr || ctx->renderContext == nullptr || bytes == nullptr ||
        len == 0)
    {
        set_error("rive_file_load: invalid arguments");
        return nullptr;
    }

    rive::ImportResult result = rive::ImportResult::malformed;
    // The RenderContext IS-A rive::Factory.
    rive::rcp<rive::File> file = rive::File::import(
        rive::Span<const uint8_t>(bytes, len),
        ctx->renderContext.get(),
        &result);
    if (file == nullptr || result != rive::ImportResult::success)
    {
        set_error(result == rive::ImportResult::unsupportedVersion
                      ? "rive::File::import: unsupported version"
                      : "rive::File::import: malformed file");
        return nullptr;
    }

    auto handle = new (std::nothrow) RiveFile();
    if (handle == nullptr)
    {
        set_error("out of memory allocating RiveFile");
        return nullptr;
    }
    handle->file = std::move(file);
    return handle;
}

extern "C" void rive_file_destroy(RiveFile* file)
{
    if (file == nullptr)
        return;
    file->file = nullptr;
    delete file;
}

extern "C" RiveArtboard* rive_file_artboard_default(RiveFile* file)
{
    if (file == nullptr || file->file == nullptr)
    {
        set_error("rive_file_artboard_default: invalid file");
        return nullptr;
    }
    std::unique_ptr<rive::ArtboardInstance> ab = file->file->artboardDefault();
    if (ab == nullptr)
    {
        set_error("rive::File::artboardDefault returned null (no artboards)");
        return nullptr;
    }
    auto handle = new (std::nothrow) RiveArtboard();
    if (handle == nullptr)
    {
        set_error("out of memory allocating RiveArtboard");
        return nullptr;
    }
    handle->artboard = std::move(ab);
    return handle;
}

extern "C" void rive_artboard_destroy(RiveArtboard* artboard)
{
    if (artboard == nullptr)
        return;
    artboard->artboard.reset();
    delete artboard;
}

extern "C" RiveStateMachine* rive_artboard_state_machine_default(
    RiveArtboard* artboard)
{
    if (artboard == nullptr || artboard->artboard == nullptr)
    {
        set_error("rive_artboard_state_machine_default: invalid artboard");
        return nullptr;
    }

    // Prefer the designer-flagged default state machine; otherwise fall back to
    // defaultScene() (first state machine, else first animation, else static).
    std::unique_ptr<rive::Scene> scene;
    if (auto sm = artboard->artboard->defaultStateMachine())
    {
        scene = std::unique_ptr<rive::Scene>(sm.release());
    }
    else
    {
        scene = artboard->artboard->defaultScene();
    }
    if (scene == nullptr)
    {
        set_error("artboard has no playable state machine, animation, or scene");
        return nullptr;
    }

    auto handle = new (std::nothrow) RiveStateMachine();
    if (handle == nullptr)
    {
        set_error("out of memory allocating RiveStateMachine");
        return nullptr;
    }
    handle->scene = std::move(scene);
    return handle;
}

extern "C" void rive_state_machine_destroy(RiveStateMachine* sm)
{
    if (sm == nullptr)
        return;
    sm->scene.reset();
    delete sm;
}

extern "C" void rive_state_machine_advance(RiveStateMachine* sm, float dt_seconds)
{
    if (sm == nullptr || sm->scene == nullptr)
        return;
    sm->scene->advanceAndApply(dt_seconds);
}

// ---------------------------------------------------------------------------
// Frame: begin -> draw -> flush.
// ---------------------------------------------------------------------------

extern "C" RiveStatus rive_frame_begin(RiveRenderContext* ctx,
                                       RiveRenderTarget* target,
                                       float r, float g, float b, float a)
{
    if (ctx == nullptr || ctx->renderContext == nullptr || target == nullptr ||
        target->sync == nullptr)
    {
        set_error("rive_frame_begin: invalid arguments");
        return 1;
    }
    if (ctx->currentRenderer != nullptr)
    {
        set_error("rive_frame_begin: a frame is already in progress");
        return 1;
    }

    if (target->sync->beginFrame() != VK_SUCCESS)
    {
        set_error("VulkanHeadlessFrameSynchronizer::beginFrame failed");
        return 1;
    }

    // Bind the offscreen image into the render target for this frame.
    target->renderTarget->setTargetImageView(target->sync->vkImageView(),
                                             target->sync->vkImage(),
                                             target->sync->lastAccess());

    RenderContext::FrameDescriptor frameDescriptor;
    frameDescriptor.renderTargetWidth = target->width;
    frameDescriptor.renderTargetHeight = target->height;
    frameDescriptor.loadAction = rive::gpu::LoadAction::clear;
    // clearColor is a straight (non-premultiplied) ARGB ColorInt.
    frameDescriptor.clearColor =
        rive::colorARGB(to_u8(a), to_u8(r), to_u8(g), to_u8(b));
    ctx->renderContext->beginFrame(frameDescriptor);

    ctx->currentRenderer = new (std::nothrow) rive::RiveRenderer(
        ctx->renderContext.get());
    if (ctx->currentRenderer == nullptr)
    {
        set_error("out of memory allocating RiveRenderer");
        return 1;
    }
    ctx->currentTarget = target;
    return RIVE_OK;
}

extern "C" RiveStatus rive_artboard_draw(RiveArtboard* artboard,
                                         RiveRenderContext* ctx)
{
    if (artboard == nullptr || artboard->artboard == nullptr || ctx == nullptr ||
        ctx->currentRenderer == nullptr || ctx->currentTarget == nullptr)
    {
        set_error("rive_artboard_draw: no frame in progress or invalid args");
        return 1;
    }

    const RiveRenderTarget* target = ctx->currentTarget;
    const rive::AABB frame(0.0f,
                           0.0f,
                           static_cast<float>(target->width),
                           static_cast<float>(target->height));
    const rive::Mat2D m = rive::computeAlignment(rive::Fit::contain,
                                                 rive::Alignment::center,
                                                 frame,
                                                 artboard->artboard->bounds());
    ctx->currentRenderer->save();
    ctx->currentRenderer->transform(m);
    artboard->artboard->draw(ctx->currentRenderer);
    ctx->currentRenderer->restore();
    return RIVE_OK;
}

extern "C" RiveStatus rive_frame_flush(RiveRenderContext* ctx)
{
    if (ctx == nullptr || ctx->renderContext == nullptr ||
        ctx->currentTarget == nullptr)
    {
        set_error("rive_frame_flush: no frame in progress");
        return 1;
    }
    RiveRenderTarget* target = ctx->currentTarget;

    RenderContext::FlushResources flushResources;
    flushResources.renderTarget = target->renderTarget.get();
    flushResources.externalCommandBuffer = target->sync->currentCommandBuffer();
    flushResources.currentFrameNumber = target->sync->currentFrameNumber();
    flushResources.safeFrameNumber = target->sync->safeFrameNumber();
    ctx->renderContext->flush(flushResources);

    // Queue the readback copy, submit, then pull the pixels (waits the fence).
    ImageAccess lastAccess = target->renderTarget->targetLastAccess();
    target->sync->queueImageCopy(&lastAccess);

    RiveStatus status = RIVE_OK;
    if (target->sync->endFrame(lastAccess) != VK_SUCCESS)
    {
        set_error("VulkanHeadlessFrameSynchronizer::endFrame failed");
        status = 1;
    }
    else if (target->sync->getPixelsFromLastImageCopy(&target->pixels) !=
             VK_SUCCESS)
    {
        set_error("getPixelsFromLastImageCopy failed");
        status = 1;
    }
    else
    {
        // getPixelsFromLastImageCopy flips rows to rive's GL-style bottom-up
        // convention (rive's own PNG writer flips a second time). The Vulkan
        // backend renders top-down, so flip back to honor our top-down contract.
        flip_rows_vertically(target->pixels, target->width, target->height);
    }

    delete ctx->currentRenderer;
    ctx->currentRenderer = nullptr;
    ctx->currentTarget = nullptr;
    return status;
}

// ---------------------------------------------------------------------------
// Readback.
// ---------------------------------------------------------------------------

extern "C" RiveStatus rive_render_target_read_pixels(RiveRenderTarget* target,
                                                     uint8_t* out_rgba,
                                                     size_t out_len)
{
    if (target == nullptr || out_rgba == nullptr)
    {
        set_error("rive_render_target_read_pixels: invalid arguments");
        return 1;
    }
    const size_t expected =
        static_cast<size_t>(target->width) * target->height * 4u;
    if (out_len != expected)
    {
        set_error("rive_render_target_read_pixels: out_len mismatch");
        return 1;
    }
    if (target->pixels.size() != expected)
    {
        set_error("rive_render_target_read_pixels: no flushed frame available");
        return 1;
    }
    std::memcpy(out_rgba, target->pixels.data(), out_len);
    return RIVE_OK;
}
