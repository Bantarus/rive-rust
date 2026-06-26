/*
 * rive_shim.cpp — implementation of the M0 C ABI (see rive_shim.h).
 *
 * Bridges a flat C ABI to the real rive-runtime C++ API and rive's own
 * `rive_vk_bootstrap` helpers (whose .cpp sources are compiled into this shim,
 * since rive-runtime does not build them into a static lib).
 */

#include "rive_shim.h"
#include "rive_shim_internal.hpp" // shared handle struct (RiveArtboard) + shim_set_error

#include <chrono>
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
#include "rive/bindable_artboard.hpp" // BindableArtboard (artboard-reference data binding value source)
#include "rive/scene.hpp"
#include "rive/animation/state_machine_instance.hpp"
#include "rive/animation/linear_animation_instance.hpp" // seek/time (LinearAnimationInstance)
#include "rive/viewmodel/viewmodel_instance.hpp" // ViewModelInstance (data binding)
#include "rive/factory.hpp"
// Out-of-band asset loading (rive_file_load_with_assets).
#include "rive/file_asset_loader.hpp"   // FileAssetLoader (host asset callback)
#include "rive/assets/file_asset.hpp"   // FileAsset (name/ext/id/decode)
#include "rive/assets/image_asset.hpp"  // ImageAsset (type discriminator)
#include "rive/assets/font_asset.hpp"   // FontAsset  (type discriminator)
#include "rive/assets/audio_asset.hpp"  // AudioAsset (type discriminator)
#include "rive/simple_array.hpp"        // SimpleArray (owned copy for decode)
#include "rive/layout.hpp"             // Fit, Alignment
#include "rive/math/aabb.hpp"
#include "rive/math/mat2d.hpp"
#include "rive/math/path_types.hpp" // FillRule (rive_artboard_draw_viewport clip)
#include "rive/math/raw_path.hpp"   // RawPath::addRect (tile clip rect)
#include "rive/refcnt.hpp"             // rcp, ref_rcp
#include "rive/renderer.hpp"           // Renderer, computeAlignment
#include "rive/shapes/paint/color.hpp" // colorARGB, ColorInt
#include "rive/span.hpp"

// PLS renderer + Vulkan backend.
#include "rive/renderer/gpu.hpp" // InterlockMode, kBufferRingSize
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
using rive::gpu::vkutil::ImageView;

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

    // ---- M1b: external (wgpu-shared) tier ----------------------------------
    // When `external` is true, the instance/device unique_ptrs above stay empty
    // (wgpu owns them) and these borrowed handles drive the frame instead.
    bool external = false;
    VkInstance extInstance = VK_NULL_HANDLE;
    VkPhysicalDevice extPhysicalDevice = VK_NULL_HANDLE;
    VkDevice extDevice = VK_NULL_HANDLE;
    uint32_t extQueueFamily = 0;
    PFN_vkGetInstanceProcAddr extGetInstanceProcAddr = nullptr;
    // Per-frame command pool + a single reused command buffer + a submit fence.
    // One in-flight frame per context: submit BLOCKS on the fence before
    // returning (M1b is correctness-first; pipelining is M2), so implicit
    // reset-on-Begin of the reused command buffer is sound. The fence is
    // shim-internal — the Rust side cannot cheaply build an ash::Device to make
    // one, and every Vulkan PFN already lives in the VulkanContext dispatch
    // table. Created lazily on first submit.
    VkCommandPool extPool = VK_NULL_HANDLE;
    VkCommandBuffer extCmdBuffer = VK_NULL_HANDLE;
    VkFence extFence = VK_NULL_HANDLE;
    // Frame-number watermark carried from begin to submit.
    uint64_t extCurrentFrameNumber = 0;
    uint64_t extSafeFrameNumber = 0;
    // Interlock/PLS mode captured at beginFrame (rive's frameInterlockMode() is
    // valid only between beginFrame and flush). -1 until the first frame begins;
    // the pls_mode getter returns this so callers can query it after flush.
    int extLastInterlockMode = -1;
    // Per-frame clockwiseFillOverride (M2.0 perf lever); seeded into each
    // FrameDescriptor by rive_frame_begin_external.
    bool extClockwise = false;

    // ---- M2.0: GPU timestamp instrumentation (rive submit timing) ----------
    // A 2-slot timestamp query pool + the query PFNs (NOT in rive's VulkanContext
    // dispatch table, so resolved once via vkGetDeviceProcAddr). Set up lazily on
    // the first submit; fully defensive — if anything is unavailable,
    // extTimestampPeriod stays 0, no timestamps are written, and last_gpu_ms
    // reports -1.
    bool extGpuTimingTried = false;
    VkQueryPool extQueryPool = VK_NULL_HANDLE;
    PFN_vkCreateQueryPool extCreateQueryPool = nullptr;
    PFN_vkDestroyQueryPool extDestroyQueryPool = nullptr;
    PFN_vkGetQueryPoolResults extGetQueryPoolResults = nullptr;
    PFN_vkCmdResetQueryPool extCmdResetQueryPool = nullptr;
    PFN_vkCmdWriteTimestamp extCmdWriteTimestamp = nullptr;
    float extTimestampPeriod = 0.0f; // ns per tick; 0 => GPU timing unavailable
    double extLastGpuMs = -1.0;      // last measured rive GPU time (ms), -1 if none

    // ---- M2a: CPU sub-span timings (fence-vs-flush split) ------------------
    // Wall time (microseconds, steady_clock) of the last external frame's rive
    // CPU flush and the blocking fence wait, measured around the exact calls in
    // render_external_frame. -1 until the first frame; surfaced via getters so the
    // perf collector can attribute the ~650us submit wall to flush vs fence.
    double extLastFlushUs = -1.0;
    double extLastFenceWaitUs = -1.0;
};

struct RiveRenderTarget
{
    std::unique_ptr<rive_vkb::VulkanHeadlessFrameSynchronizer> sync;
    rive::rcp<RenderTargetVulkanImpl> renderTarget;
    uint32_t width = 0;
    uint32_t height = 0;
    std::vector<uint8_t> pixels; // last flushed frame, RGBA8 premultiplied top-down

    // ---- M1b: external (wgpu-shared) tier ----------------------------------
    // When `external` is true, `sync`/`pixels` are unused and the rive target
    // wraps a wgpu-owned VkImage (never freed here).
    bool external = false;
    VkImage extImage = VK_NULL_HANDLE;
    VkImageView extView = VK_NULL_HANDLE;
    // A view the shim created itself (when the caller passed view==0); kept alive
    // so `extView` stays valid. Null when the caller supplied the view.
    rive::rcp<ImageView> ownedView;
    // Tracked layout of `extImage` across frames: seeds setTargetImageView each
    // frame (UNDEFINED first frame, SHADER_READ_ONLY after our post-flush barrier).
    ImageAccess lastAccess{};
};

struct RiveFile
{
    rive::rcp<rive::File> file;
};

// RiveArtboard AND RiveStateMachine are defined in rive_shim_internal.hpp (shared
// with the view-model / input TUs). The remaining handle structs stay file-local
// until a second TU needs them.

// ---------------------------------------------------------------------------
// Error string.
// ---------------------------------------------------------------------------

extern "C" const char* rive_last_error(void) { return last_error_storage().c_str(); }

// Cross-TU error reporter declared in rive_shim_internal.hpp: `set_error` has
// internal linkage (anonymous namespace), so per-feature TUs route through this.
void shim_set_error(const char* msg) { set_error(msg); }

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

    if (ctx->external)
    {
        // External tier: we borrow the wgpu-owned device — never destroy it.
        // We DO own the per-frame command pool; destroy it (which frees its
        // command buffers) via the device dispatch table, which lives in the
        // VulkanContext, BEFORE resetting renderContext drops that table.
        if (ctx->impl != nullptr)
        {
            VulkanContext* vk = ctx->impl->vulkanContext();
            // The fence is unsignaled or already waited (submit blocks), so
            // destroying it here is safe. Destroy the pool last (it frees the cb).
            if (ctx->extFence != VK_NULL_HANDLE)
            {
                vk->DestroyFence(ctx->extDevice, ctx->extFence, nullptr);
                ctx->extFence = VK_NULL_HANDLE;
            }
            if (ctx->extPool != VK_NULL_HANDLE)
            {
                vk->DestroyCommandPool(ctx->extDevice, ctx->extPool, nullptr);
                ctx->extPool = VK_NULL_HANDLE;
                ctx->extCmdBuffer = VK_NULL_HANDLE;
            }
            // M2.0: the GPU-timing query pool (created via a resolved PFN, so freed
            // via the matching one). The fence wait above means no query is in
            // flight. Skipped cleanly if timing was never set up.
            if (ctx->extQueryPool != VK_NULL_HANDLE &&
                ctx->extDestroyQueryPool != nullptr)
            {
                ctx->extDestroyQueryPool(ctx->extDevice, ctx->extQueryPool, nullptr);
                ctx->extQueryPool = VK_NULL_HANDLE;
            }
        }
        ctx->renderContext.reset(); // drops the VulkanContext ref (not the device)
        delete ctx;
        return;
    }

    // Self-managed (M0/M1a) destruction order: render context (drops its
    // VulkanContext ref), then device, then instance. Any RiveRenderTarget must
    // already be destroyed.
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
    // External tier: the VkImage is wgpu-owned (never freed here). Drop any
    // shim-created image view (the rcp releases it through the VulkanContext)
    // and the rive target wrapper. No synchronizer.
    if (target->external)
    {
        target->ownedView = nullptr;
        target->renderTarget = nullptr;
        delete target;
        return;
    }
    // Self-managed: destroy the synchronizer first (it waits for in-flight
    // command buffers), then drop the render target wrapper.
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

// A FileAssetLoader that defers to a host C callback. Used by
// rive_file_load_with_assets to let the game supply out-of-band (Referenced)
// images / fonts / audio. Heap-allocated and refcounted: File::import stores an
// rcp to it for the File's lifetime, so it must NOT be a stack object (it would
// be double-freed when the File drops). In practice assets resolve synchronously
// during import, so the host callback only fires within the load call.
namespace {
class CallbackAssetLoader : public rive::FileAssetLoader
{
public:
    CallbackAssetLoader(RiveAssetLoadFn fn, void* user) : m_fn(fn), m_user(user)
    {}

    bool loadContents(rive::FileAsset& asset,
                      rive::Span<const uint8_t> inBandBytes,
                      rive::Factory* factory) override
    {
        // The c_str() pointers below alias these locals; keep them in scope for
        // the whole callback (the host must not retain them past the call).
        const std::string name = asset.name();
        const std::string ext = asset.fileExtension();
        const std::string uuid = asset.cdnUuidStr();

        uint16_t type = RIVE_ASSET_OTHER;
        if (asset.is<rive::ImageAsset>())
            type = RIVE_ASSET_IMAGE;
        else if (asset.is<rive::FontAsset>())
            type = RIVE_ASSET_FONT;
        else if (asset.is<rive::AudioAsset>())
            type = RIVE_ASSET_AUDIO;

        RiveAssetRequest req;
        req.name = name.c_str();
        req.file_extension = ext.c_str();
        req.cdn_uuid = uuid.c_str();
        req.asset_id = asset.assetId();
        req.asset_type = type;
        req.in_band_bytes = inBandBytes.size() > 0 ? inBandBytes.data() : nullptr;
        req.in_band_len = inBandBytes.size();

        const uint8_t* out = nullptr;
        size_t out_len = 0;
        if (m_fn(m_user, &req, &out, &out_len) == 0 || out == nullptr ||
            out_len == 0)
        {
            return false; // host declined → caller falls back to in-band bytes
        }

        // Copy the host bytes into an owned array, then let the asset's own
        // decoder (image/font/audio) turn them into a render resource via the
        // factory (the RenderContext — has libpng/jpeg/webp + harfbuzz built in).
        rive::SimpleArray<uint8_t> data(out, out_len);
        return asset.decode(data, factory);
    }

private:
    RiveAssetLoadFn m_fn;
    void* m_user;
};
} // namespace

// Shared import + handle-wrap for both file-load entry points. `loader` may be a
// null rcp (no out-of-band loading). Imports via File::import (RenderContext
// IS-A rive::Factory); File takes its own rcp on `loader`, so ownership is clean.
static RiveFile* load_file_impl(RiveRenderContext* ctx,
                                const uint8_t* bytes,
                                size_t len,
                                rive::rcp<rive::FileAssetLoader> loader)
{
    if (ctx == nullptr || ctx->renderContext == nullptr || bytes == nullptr ||
        len == 0)
    {
        set_error("rive_file_load: invalid arguments");
        return nullptr;
    }

    rive::ImportResult result = rive::ImportResult::malformed;
    rive::rcp<rive::File> file = rive::File::import(
        rive::Span<const uint8_t>(bytes, len),
        ctx->renderContext.get(),
        &result,
        std::move(loader));
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

extern "C" RiveFile* rive_file_load(RiveRenderContext* ctx,
                                    const uint8_t* bytes,
                                    size_t len)
{
    return load_file_impl(ctx, bytes, len, nullptr);
}

extern "C" RiveFile* rive_file_load_with_assets(RiveRenderContext* ctx,
                                                const uint8_t* bytes,
                                                size_t len,
                                                RiveAssetLoadFn load_fn,
                                                void* user)
{
    rive::rcp<rive::FileAssetLoader> loader =
        load_fn != nullptr
            ? rive::make_rcp<CallbackAssetLoader>(load_fn, user)
            : nullptr;
    return load_file_impl(ctx, bytes, len, std::move(loader));
}

extern "C" void rive_file_destroy(RiveFile* file)
{
    if (file == nullptr)
        return;
    file->file = nullptr;
    delete file;
}

// Decodes encoded image bytes (PNG/JPEG/WEBP — the same decoders the asset loader
// uses) into an OWNED RiveImage, the value source for image-property data binding
// (rive_artboard_vm_set_image / rive_vmi_set_image). Decoding goes through the
// render context's rive::Factory, so the resulting RenderImage is bound to THIS
// context's device and must only be bound into artboards on the same context.
// Returns NULL (+ error) on bad arguments or a decode failure. Free with
// rive_image_destroy; binding takes its own ref, so it may be freed after binding.
extern "C" RiveImage* rive_image_decode(RiveRenderContext* ctx,
                                        const uint8_t* bytes,
                                        size_t len)
{
    if (ctx == nullptr || ctx->renderContext == nullptr || bytes == nullptr ||
        len == 0)
    {
        set_error("rive_image_decode: invalid arguments");
        return nullptr;
    }
    rive::rcp<rive::RenderImage> image =
        ctx->renderContext->decodeImage(rive::Span<const uint8_t>(bytes, len));
    if (image == nullptr)
    {
        set_error("rive_image_decode: could not decode image bytes");
        return nullptr;
    }
    auto handle = new (std::nothrow) RiveImage();
    if (handle == nullptr)
    {
        set_error("out of memory allocating RiveImage");
        return nullptr;
    }
    handle->image = std::move(image);
    return handle;
}

extern "C" void rive_image_destroy(RiveImage* image)
{
    if (image == nullptr)
        return;
    image->image = nullptr;
    delete image;
}

// Wraps a freshly-instanced ArtboardInstance into a RiveArtboard handle, binding
// its DEFAULT view-model instance. Shared by the default / named / by-index
// selectors so all three get identical data-binding + scripting setup. Takes
// ownership of `ab`; `ab == nullptr` means the caller's lookup missed (the caller
// has already set the specific error), so this just forwards null.
static RiveArtboard* make_artboard_handle(rive::File* file,
                                          std::unique_ptr<rive::ArtboardInstance> ab)
{
    if (ab == nullptr)
        return nullptr;
    auto handle = new (std::nothrow) RiveArtboard();
    if (handle == nullptr)
    {
        set_error("out of memory allocating RiveArtboard");
        return nullptr;
    }
    handle->artboard = std::move(ab);

    // Bind the artboard's DEFAULT view-model instance so editor-authored data
    // bindings resolve at runtime — including a script's view-model `Input<...>`
    // wired to a view model in the editor. Without a data context those inputs
    // are nil and the script errors ("attempt to index nil"). This MUST run
    // before the state machine is instanced: the SM clones scripted objects with
    // the artboard's data context. `createDefaultViewModelInstance` returns null
    // for artboards with no view model, so non-data-bound content is unchanged.
    if (auto vmi = file->createDefaultViewModelInstance(handle->artboard.get()))
    {
        handle->artboard->bindViewModelInstance(vmi);
        handle->vmInstance = std::move(vmi);
        // Wrap the SAME bound instance for name-based property get/set (data
        // binding). The public ctor just holds the rcp — no new instance, so the
        // script/data-binding context above is untouched (see data-binding.mdx).
        handle->vmRuntime =
            rive::make_rcp<rive::ViewModelInstanceRuntime>(handle->vmInstance);
    }
    return handle;
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
    return make_artboard_handle(file->file.get(), std::move(ab));
}

extern "C" RiveArtboard* rive_file_artboard_named(RiveFile* file, const char* name)
{
    if (file == nullptr || file->file == nullptr)
    {
        set_error("rive_file_artboard_named: invalid file");
        return nullptr;
    }
    if (name == nullptr)
    {
        set_error("rive_file_artboard_named: name is null");
        return nullptr;
    }
    std::unique_ptr<rive::ArtboardInstance> ab = file->file->artboardNamed(name);
    if (ab == nullptr)
    {
        set_error("rive::File::artboardNamed found no artboard with that name");
        return nullptr;
    }
    return make_artboard_handle(file->file.get(), std::move(ab));
}

extern "C" RiveArtboard* rive_file_artboard_at(RiveFile* file, uint32_t index)
{
    if (file == nullptr || file->file == nullptr)
    {
        set_error("rive_file_artboard_at: invalid file");
        return nullptr;
    }
    std::unique_ptr<rive::ArtboardInstance> ab = file->file->artboardAt(index);
    if (ab == nullptr)
    {
        set_error("rive::File::artboardAt index out of range");
        return nullptr;
    }
    return make_artboard_handle(file->file.get(), std::move(ab));
}

// Copies `s` into a caller buffer per the two-call protocol: sets *out_len to the
// full length (call with cap=0 to size first), copies min(cap, len) bytes with NO
// NUL terminator (caller slices to *out_len). Used by the selection introspection.
static void copy_name(const std::string& s, char* buf, size_t cap, size_t* out_len)
{
    if (out_len != nullptr)
        *out_len = s.size();
    if (buf != nullptr && cap > 0)
        std::memcpy(buf, s.data(), s.size() < cap ? s.size() : cap);
}

// Selection introspection: discover the names a ByName/ByIndex selector can pick.
extern "C" uint32_t rive_file_artboard_count(RiveFile* file)
{
    if (file == nullptr || file->file == nullptr)
        return 0;
    return static_cast<uint32_t>(file->file->artboardCount());
}

extern "C" RiveStatus rive_file_artboard_name_at(RiveFile* file, uint32_t index,
                                                 char* buf, size_t cap, size_t* out_len)
{
    if (file == nullptr || file->file == nullptr)
    {
        set_error("rive_file_artboard_name_at: invalid file");
        return 1;
    }
    if (index >= file->file->artboardCount())
    {
        set_error("rive_file_artboard_name_at: index out of range");
        return 1;
    }
    copy_name(file->file->artboardNameAt(index), buf, cap, out_len);
    return RIVE_OK;
}

extern "C" void rive_artboard_destroy(RiveArtboard* artboard)
{
    if (artboard == nullptr)
        return;
    artboard->artboard.reset();
    delete artboard;
}

// --- Artboard-reference data binding: BindableArtboard value source ----------
// A BindableArtboard wraps an ArtboardInstance pulled from this File (holding the
// File alive) so it can be bound to a propertyArtboard view-model property (see
// rive_artboard_vm_set_artboard / rive_vmi_set_artboard in the view-model TU). The
// owned RiveBindableArtboard handle is the artboard analogue of RiveImage; free it
// with rive_bindable_artboard_destroy (binding takes its own ref, so it may be
// freed after binding).
static RiveBindableArtboard* wrap_bindable(rive::rcp<rive::BindableArtboard> ba)
{
    if (ba == nullptr)
        return nullptr; // caller set the not-found error
    auto* h = new (std::nothrow) RiveBindableArtboard();
    if (h == nullptr)
    {
        set_error("out of memory allocating RiveBindableArtboard");
        return nullptr;
    }
    h->bindable = std::move(ba);
    return h;
}

extern "C" RiveBindableArtboard* rive_file_bindable_artboard_named(RiveFile* file,
                                                                   const char* name)
{
    if (file == nullptr || file->file == nullptr || name == nullptr)
    {
        set_error("rive_file_bindable_artboard_named: invalid file or name");
        return nullptr;
    }
    rive::rcp<rive::BindableArtboard> ba = file->file->bindableArtboardNamed(name);
    if (ba == nullptr)
        set_error("rive::File::bindableArtboardNamed found no artboard with that name");
    return wrap_bindable(std::move(ba));
}

extern "C" RiveBindableArtboard* rive_file_bindable_artboard_default(RiveFile* file)
{
    if (file == nullptr || file->file == nullptr)
    {
        set_error("rive_file_bindable_artboard_default: invalid file");
        return nullptr;
    }
    rive::rcp<rive::BindableArtboard> ba = file->file->bindableArtboardDefault();
    if (ba == nullptr)
        set_error("rive::File::bindableArtboardDefault returned null (no artboards)");
    return wrap_bindable(std::move(ba));
}

extern "C" void rive_bindable_artboard_destroy(RiveBindableArtboard* bindable)
{
    if (bindable == nullptr)
        return;
    bindable->bindable = nullptr;
    delete bindable;
}

// Wraps a Scene (state machine / animation) into a RiveStateMachine handle,
// binding the artboard's view-model instance to it. Shared by the default / named
// / by-index selectors. Takes ownership of `scene`; `scene == nullptr` means the
// caller's lookup missed (error already set), so this forwards null. `isLinear`
// records the concrete type (the caller knows it statically) for the seek API;
// `smInstance` is the SAME object as `scene` typed as a StateMachineInstance (null
// for the animation fallback) so the input TU can reach focus / keyboard / gamepad
// without an RTTI downcast.
static RiveStateMachine* make_sm_handle(RiveArtboard* artboard,
                                        std::unique_ptr<rive::Scene> scene,
                                        bool isLinear,
                                        rive::StateMachineInstance* smInstance)
{
    if (scene == nullptr)
        return nullptr;
    auto handle = new (std::nothrow) RiveStateMachine();
    if (handle == nullptr)
    {
        set_error("out of memory allocating RiveStateMachine");
        return nullptr;
    }
    handle->scene = std::move(scene);
    handle->isLinear = isLinear;
    handle->smInstance = smInstance;

    // Bind the SAME view-model instance to the state machine too (per the
    // data-binding contract: the artboard binding drives layout-affecting
    // properties; the SM binding drives transitions + listener conditions).
    // No-op when the artboard had no view model.
    if (artboard->vmInstance)
    {
        handle->scene->bindViewModelInstance(artboard->vmInstance);
    }
    return handle;
}

extern "C" RiveStateMachine* rive_artboard_state_machine_default(
    RiveArtboard* artboard)
{
    if (artboard == nullptr || artboard->artboard == nullptr)
    {
        set_error("rive_artboard_state_machine_default: invalid artboard");
        return nullptr;
    }

    // Replicate rive's defaultScene() chain (defaultStateMachine → stateMachineAt(0)
    // → animationAt(0)) EXPLICITLY, rather than calling the type-erasing
    // defaultScene(), so we know the concrete type and can tag `isLinear` definitively
    // for the seek API. The animation branch is the only LinearAnimationInstance; both
    // SM branches are not seekable. (defaultScene() never yields a StaticScene in this
    // runtime — a static-only artboard returns null here, exactly as before.)
    std::unique_ptr<rive::Scene> scene;
    bool isLinear = false;
    rive::StateMachineInstance* smInstance = nullptr;
    if (auto sm = artboard->artboard->defaultStateMachine())
    {
        smInstance = sm.get(); // alias survives the release() into `scene`
        scene = std::unique_ptr<rive::Scene>(sm.release());
    }
    else if (auto sm = artboard->artboard->stateMachineAt(0))
    {
        smInstance = sm.get();
        scene = std::unique_ptr<rive::Scene>(sm.release());
    }
    else if (auto anim = artboard->artboard->animationAt(0))
    {
        scene = std::unique_ptr<rive::Scene>(anim.release());
        isLinear = true;
    }
    if (scene == nullptr)
    {
        set_error("artboard has no playable state machine, animation, or scene");
        return nullptr;
    }
    return make_sm_handle(artboard, std::move(scene), isLinear, smInstance);
}

extern "C" RiveStateMachine* rive_artboard_state_machine_named(
    RiveArtboard* artboard, const char* name)
{
    if (artboard == nullptr || artboard->artboard == nullptr)
    {
        set_error("rive_artboard_state_machine_named: invalid artboard");
        return nullptr;
    }
    if (name == nullptr)
    {
        set_error("rive_artboard_state_machine_named: name is null");
        return nullptr;
    }
    // Named lookup is a state machine ONLY (no animation/static fallback): the
    // caller asked for a specific SM, so a miss is an error, not a silent default.
    std::unique_ptr<rive::StateMachineInstance> sm =
        artboard->artboard->stateMachineNamed(name);
    if (sm == nullptr)
    {
        set_error("artboard has no state machine with that name");
        return nullptr;
    }
    // A named scene is always a StateMachineInstance — not seekable (isLinear=false).
    rive::StateMachineInstance* smInstance = sm.get();
    return make_sm_handle(artboard, std::unique_ptr<rive::Scene>(sm.release()), false, smInstance);
}

extern "C" RiveStateMachine* rive_artboard_state_machine_at(
    RiveArtboard* artboard, uint32_t index)
{
    if (artboard == nullptr || artboard->artboard == nullptr)
    {
        set_error("rive_artboard_state_machine_at: invalid artboard");
        return nullptr;
    }
    std::unique_ptr<rive::StateMachineInstance> sm =
        artboard->artboard->stateMachineAt(index);
    if (sm == nullptr)
    {
        set_error("state machine index out of range");
        return nullptr;
    }
    // An indexed scene is always a StateMachineInstance — not seekable (isLinear=false).
    rive::StateMachineInstance* smInstance = sm.get();
    return make_sm_handle(artboard, std::unique_ptr<rive::Scene>(sm.release()), false, smInstance);
}

// Selection introspection: discover the state-machine names selectable by name/index.
extern "C" uint32_t rive_artboard_state_machine_count(RiveArtboard* artboard)
{
    if (artboard == nullptr || artboard->artboard == nullptr)
        return 0;
    return static_cast<uint32_t>(artboard->artboard->stateMachineCount());
}

extern "C" RiveStatus rive_artboard_state_machine_name_at(RiveArtboard* artboard, uint32_t index,
                                                          char* buf, size_t cap, size_t* out_len)
{
    if (artboard == nullptr || artboard->artboard == nullptr)
    {
        set_error("rive_artboard_state_machine_name_at: invalid artboard");
        return 1;
    }
    if (index >= artboard->artboard->stateMachineCount())
    {
        set_error("rive_artboard_state_machine_name_at: index out of range");
        return 1;
    }
    copy_name(artboard->artboard->stateMachineNameAt(index), buf, cap, out_len);
    return RIVE_OK;
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

// Playback duration in seconds: the animation length for a LINEAR-ANIMATION scene,
// or -1 for a STATE MACHINE (rive::StateMachineInstance::durationSeconds() returns
// -1 — state machines are continuous, no fixed length). The safe layer maps -1 to
// None. Also -1 on a null handle. Read it to bound a seek (the seekable range).
extern "C" float rive_state_machine_duration(RiveStateMachine* sm)
{
    if (sm == nullptr || sm->scene == nullptr)
        return -1.0f;
    return sm->scene->durationSeconds();
}

// Returns the scene as a LinearAnimationInstance, or nullptr if it is not one (a
// state machine, etc.). The runtime is built with -fno-rtti, so dynamic_cast is
// unavailable; instead the selectors recorded the concrete type in `isLinear` at
// construction (where it is statically known), which makes this static_cast sound.
// (`Scene` is the first base of LinearAnimationInstance, so the downcast is a
// zero-offset adjustment.) Caller must have checked sm/scene non-null. Note we do
// NOT discriminate on durationSeconds(): a StaticScene returns 0, aliasing a real
// zero-length animation, so the duration sentinel could not tell them apart.
static rive::LinearAnimationInstance* as_linear(RiveStateMachine* sm)
{
    if (!sm->isLinear)
        return nullptr;
    return static_cast<rive::LinearAnimationInstance*>(sm->scene.get());
}

// Current playhead position in seconds for a LINEAR-ANIMATION scene, or -1 if the
// scene is a state machine (no scalar playhead) or the handle is null.
extern "C" float rive_state_machine_time(RiveStateMachine* sm)
{
    if (sm == nullptr || sm->scene == nullptr)
        return -1.0f;
    if (auto* lai = as_linear(sm))
        return lai->time();
    return -1.0f;
}

// Seek a LINEAR-ANIMATION scene to absolute time `t` (seconds), clamped to
// [0, duration]. Returns true if the scene is seekable (a LinearAnimationInstance)
// and the seek applied, false otherwise — state machines have no scalar playhead
// and cannot be sought (return false; the caller no-ops). Applies immediately
// (advanceAndApply(0)) so the seeked pose is visible WITHOUT a subsequent advance
// (e.g. seeking while paused). `time(t)` only sets the playhead; the apply pushes
// it through the animation onto the artboard hierarchy.
extern "C" bool rive_state_machine_seek(RiveStateMachine* sm, float t)
{
    if (sm == nullptr || sm->scene == nullptr)
        return false;
    auto* lai = as_linear(sm);
    if (lai == nullptr)
        return false;
    // Clamp into [0, duration]; guard against a non-finite request.
    if (!(t == t)) // NaN
        t = 0.0f;
    const float dur = lai->durationSeconds();
    if (t < 0.0f)
        t = 0.0f;
    else if (dur > 0.0f && t > dur)
        t = dur;
    lai->time(t);
    lai->advanceAndApply(0.0f);
    return true;
}

// Sets the fit/alignment used to INVERT pointer coords (must match the artboard's
// draw fit/alignment — the RiveFit component sets both). `fit` is a Fit ordinal
// (fill=0 contain=1 cover=2 fitWidth=3 fitHeight=4 none=5 scaleDown=6 layout=7);
// out-of-range falls back to contain. align_x/_y are -1..1.
extern "C" void rive_state_machine_set_fit_align(RiveStateMachine* sm, uint32_t fit,
                                                 float align_x, float align_y,
                                                 float scale_factor)
{
    if (sm == nullptr)
        return;
    sm->fit = fit <= static_cast<uint32_t>(rive::Fit::layout)
                  ? static_cast<rive::Fit>(fit)
                  : rive::Fit::contain;
    sm->alignment = rive::Alignment(align_x, align_y);
    sm->scaleFactor = scale_factor;
}

// Sets the DRAWN tile size (px) for ATLAS pointer inversion. An atlas face draws
// into a gutter-inset tile sub-rect via rive_artboard_draw_viewport, so its
// fit/alignment maps the artboard into the tile — NOT the full target. Given the
// tile's drawn `tile_w`×`tile_h`, the four pointer fns normalize the incoming
// target-space coords into the tile before inverting (the tile OFFSET cancels, so
// only the size is needed). Pass (0, 0) — or any non-positive — to restore
// full-target inversion (the dedicated-face path). Set per-frame by the atlas node.
extern "C" void rive_state_machine_set_pointer_tile(RiveStateMachine* sm,
                                                    float tile_w, float tile_h)
{
    if (sm == nullptr)
        return;
    sm->ptrTileW = tile_w;
    sm->ptrTileH = tile_h;
}

// ---------------------------------------------------------------------------
// Pointer input -> state-machine Listeners (eye/head joysticks, buttons, hover).
// ---------------------------------------------------------------------------

// Map a pointer in TARGET-PIXEL space (0..w, 0..h, top-left origin) into the
// artboard's local space by INVERTING the same alignment the draw used. Uses the
// state machine's stored fit/alignment (kept in sync with the artboard's by the
// RiveFit component) — MUST match the draw transform or hits won't line up with
// the rendered pixels. `scene->bounds()` is the artboard's {0,0,w,h}.
//
// Two draw paths, one inversion:
//   * Dedicated (ptrTile == 0): the artboard is fit into the FULL target via
//     rive_artboard_draw, so invert against frame {0,0,w,h} with the point as-is.
//   * Atlas (ptrTile > 0): the artboard is fit into a TILE via draw_viewport, so
//     invert against the tile {0,0,tileW,tileH} and normalize the pointer into it
//     (the tile offset cancels under computeAlignment, so only the size matters).
// The dedicated branch keeps the point exactly {x,y} → byte-identical to before.
static rive::Vec2D pointer_to_artboard(RiveStateMachine* sm,
                                       float x, float y, float w, float h)
{
    const bool tiled = sm->ptrTileW > 0.0f && sm->ptrTileH > 0.0f;
    const float fw = tiled ? sm->ptrTileW : w;
    const float fh = tiled ? sm->ptrTileH : h;
    const rive::AABB frame(0.0f, 0.0f, fw, fh);
    const rive::Mat2D m = rive::computeAlignment(sm->fit,
                                                 sm->alignment,
                                                 frame,
                                                 sm->scene->bounds(),
                                                 sm->scaleFactor);
    // Callers guarantee w,h > 0 (the pointer fns reject non-positive), so the
    // normalization is divide-safe.
    const rive::Vec2D pt =
        tiled ? rive::Vec2D{x / w * fw, y / h * fh} : rive::Vec2D{x, y};
    return m.invertOrIdentity() * pt;
}

// The four pointer events. `x,y` are in target-pixel space; `w,h` are the
// render-target pixel size those coords are relative to (the same W/H passed to
// the offscreen target). Returns rive::HitResult as a byte: 0 none / 1 hit /
// 2 hitOpaque (and 0 on null/degenerate args). `!(w > 0)` also rejects NaN.
extern "C" uint8_t rive_state_machine_pointer_move(RiveStateMachine* sm,
                                                   float x, float y,
                                                   float w, float h)
{
    if (sm == nullptr || sm->scene == nullptr || !(w > 0.0f) || !(h > 0.0f))
        return 0;
    return static_cast<uint8_t>(
        sm->scene->pointerMove(pointer_to_artboard(sm, x, y, w, h), 0.0f));
}

extern "C" uint8_t rive_state_machine_pointer_down(RiveStateMachine* sm,
                                                   float x, float y,
                                                   float w, float h)
{
    if (sm == nullptr || sm->scene == nullptr || !(w > 0.0f) || !(h > 0.0f))
        return 0;
    return static_cast<uint8_t>(
        sm->scene->pointerDown(pointer_to_artboard(sm, x, y, w, h)));
}

extern "C" uint8_t rive_state_machine_pointer_up(RiveStateMachine* sm,
                                                 float x, float y,
                                                 float w, float h)
{
    if (sm == nullptr || sm->scene == nullptr || !(w > 0.0f) || !(h > 0.0f))
        return 0;
    return static_cast<uint8_t>(
        sm->scene->pointerUp(pointer_to_artboard(sm, x, y, w, h)));
}

extern "C" uint8_t rive_state_machine_pointer_exit(RiveStateMachine* sm,
                                                   float x, float y,
                                                   float w, float h)
{
    if (sm == nullptr || sm->scene == nullptr || !(w > 0.0f) || !(h > 0.0f))
        return 0;
    return static_cast<uint8_t>(
        sm->scene->pointerExit(pointer_to_artboard(sm, x, y, w, h)));
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
    // Capture the interlock/PLS mode now — valid only between beginFrame and
    // flush. The pls_mode getter returns this cached value after the frame.
    ctx->extLastInterlockMode =
        static_cast<int>(ctx->renderContext->frameInterlockMode());

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

// Sets how the artboard aligns into its draw target (full-target draw + atlas-tile
// draw_viewport both read this). `fit` is a Fit ordinal (fill=0 contain=1 cover=2
// fitWidth=3 fitHeight=4 none=5 scaleDown=6 layout=7); out-of-range falls back to
// contain. align_x/_y are -1..1 (center=0,0; bottomCenter=0,1). scale_factor is
// used only by Fit::layout. Default (contain/center/1.0) == the historical draw.
extern "C" void rive_artboard_set_fit_align(RiveArtboard* artboard, uint32_t fit,
                                            float align_x, float align_y,
                                            float scale_factor)
{
    if (artboard == nullptr)
        return;
    artboard->fit = fit <= static_cast<uint32_t>(rive::Fit::layout)
                        ? static_cast<rive::Fit>(fit)
                        : rive::Fit::contain;
    artboard->alignment = rive::Alignment(align_x, align_y);
    artboard->scaleFactor = scale_factor;
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
    const rive::Mat2D m = rive::computeAlignment(artboard->fit,
                                                 artboard->alignment,
                                                 frame,
                                                 artboard->artboard->bounds(),
                                                 artboard->scaleFactor);
    ctx->currentRenderer->save();
    ctx->currentRenderer->transform(m);
    artboard->artboard->draw(ctx->currentRenderer);
    ctx->currentRenderer->restore();
    return RIVE_OK;
}

extern "C" RiveStatus rive_artboard_draw_viewport(RiveArtboard* artboard,
                                                  RiveRenderContext* ctx,
                                                  float x, float y,
                                                  float w, float h)
{
    if (artboard == nullptr || artboard->artboard == nullptr || ctx == nullptr ||
        ctx->currentRenderer == nullptr || ctx->currentTarget == nullptr ||
        ctx->renderContext == nullptr)
    {
        set_error("rive_artboard_draw_viewport: no frame in progress or invalid args");
        return 1;
    }
    // Inverse logic also rejects NaN (a non-finite compare is false).
    if (!(w > 0.0f) || !(h > 0.0f))
    {
        set_error("rive_artboard_draw_viewport: width/height must be > 0");
        return 1;
    }

    rive::RiveRenderer* r = ctx->currentRenderer;
    // The tile sub-rect in ATLAS PIXEL space (x,y = top-left; w,h = tile size).
    const rive::AABB tile(x, y, x + w, y + h);
    // Fit the artboard's content bounds into the tile (same Fit/Alignment as the
    // full-target rive_artboard_draw, but the frame is the tile, not the target).
    const rive::Mat2D m = rive::computeAlignment(artboard->fit,
                                                 artboard->alignment,
                                                 tile,
                                                 artboard->artboard->bounds(),
                                                 artboard->scaleFactor);
    r->save();
    // CLIP FIRST, while the matrix is still IDENTITY: clipPath captures the current
    // stack matrix as the clipRect matrix, so the tile rect stays in atlas-pixel
    // space, independent of the artboard's own overflow. An axis-aligned rect path
    // (RawPath::addRect = move+3line+close) routes through rive's cheap clipRect
    // shader path (NO clip-mask draw, NO clip-ID, no interaction with the PLS
    // coverage machinery). REQUIRED: rive only bounds-culls against the whole target,
    // so overflow content (strokes, feather, overflow shapes) would otherwise bleed
    // into neighbor tiles in a shared atlas. (Build the RawPath explicitly: the
    // Factory `makeRenderPath(AABB)` convenience is name-hidden by RenderContext's
    // `makeRenderPath(RawPath&, FillRule)` override.)
    rive::RawPath clipRect;
    clipRect.addRect(tile);
    auto clip = ctx->renderContext->makeRenderPath(clipRect, rive::FillRule::nonZero);
    r->clipPath(clip.get());
    // THEN place the artboard into the tile and draw, clipped to it.
    r->transform(m);
    artboard->artboard->draw(r);
    r->restore();
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

// ===========================================================================
// M1b: external (wgpu-shared) Vulkan tier.
// ===========================================================================

// gpu::InterlockMode ordinals are part of the C ABI (RIVE_PLS_* in the header).
// Pin them so a rive-runtime bump that reorders the enum fails the build here.
static_assert(static_cast<int>(rive::gpu::InterlockMode::rasterOrdering) == RIVE_PLS_RASTER_ORDERING);
static_assert(static_cast<int>(rive::gpu::InterlockMode::atomics) == RIVE_PLS_ATOMICS);
static_assert(static_cast<int>(rive::gpu::InterlockMode::clockwise) == RIVE_PLS_CLOCKWISE);
static_assert(static_cast<int>(rive::gpu::InterlockMode::clockwiseAtomic) == RIVE_PLS_CLOCKWISE_ATOMIC);
static_assert(static_cast<int>(rive::gpu::InterlockMode::msaa) == RIVE_PLS_MSAA);
static_assert(rive::gpu::INTERLOCK_MODE_COUNT == 5);

namespace {

// Vulkan handles cross the C ABI as uint64_t. On 64-bit hosts (the only target;
// see the header) dispatchable handles are pointers and non-dispatchable handles
// are either pointers or uint64_t — `(H)(uintptr_t)v` is correct for both.
template <class H>
H handle_from_u64(uint64_t v)
{
    return reinterpret_cast<H>(static_cast<uintptr_t>(v));
}
template <class H>
uint64_t handle_to_u64(H h)
{
    return static_cast<uint64_t>(reinterpret_cast<uintptr_t>(h));
}

// Lazily creates the per-frame command pool + a single reused command buffer +
// the submit fence on the external context's queue family. Returns false (and
// sets the error) on failure. The pool uses RESET_COMMAND_BUFFER so
// vkBeginCommandBuffer implicitly resets the buffer each frame (one in-flight
// frame per context; submit blocks on the fence before returning). The fence is
// created UNSIGNALED; submit resets-then-waits it.
bool ensure_ext_frame_objects(RiveRenderContext* ctx, VulkanContext* vk)
{
    if (ctx->extCmdBuffer != VK_NULL_HANDLE && ctx->extFence != VK_NULL_HANDLE)
    {
        return true;
    }
    if (ctx->extPool == VK_NULL_HANDLE)
    {
        VkCommandPoolCreateInfo poolInfo{};
        poolInfo.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO;
        poolInfo.flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT;
        poolInfo.queueFamilyIndex = ctx->extQueueFamily;
        if (vk->CreateCommandPool(ctx->extDevice, &poolInfo, nullptr, &ctx->extPool) !=
            VK_SUCCESS)
        {
            set_error("rive external: vkCreateCommandPool failed");
            ctx->extPool = VK_NULL_HANDLE;
            return false;
        }
    }
    if (ctx->extCmdBuffer == VK_NULL_HANDLE)
    {
        VkCommandBufferAllocateInfo allocInfo{};
        allocInfo.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO;
        allocInfo.commandPool = ctx->extPool;
        allocInfo.level = VK_COMMAND_BUFFER_LEVEL_PRIMARY;
        allocInfo.commandBufferCount = 1;
        if (vk->AllocateCommandBuffers(ctx->extDevice, &allocInfo, &ctx->extCmdBuffer) !=
            VK_SUCCESS)
        {
            set_error("rive external: vkAllocateCommandBuffers failed");
            ctx->extCmdBuffer = VK_NULL_HANDLE;
            return false;
        }
    }
    if (ctx->extFence == VK_NULL_HANDLE)
    {
        VkFenceCreateInfo fenceInfo{};
        fenceInfo.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO;
        fenceInfo.flags = 0; // unsignaled
        if (vk->CreateFence(ctx->extDevice, &fenceInfo, nullptr, &ctx->extFence) !=
            VK_SUCCESS)
        {
            set_error("rive external: vkCreateFence failed");
            ctx->extFence = VK_NULL_HANDLE;
            return false;
        }
    }
    return true;
}

// Lazily sets up GPU timestamp instrumentation (M2.0): resolves the query PFNs
// (not in rive's dispatch table) via vkGetDeviceProcAddr, creates a 2-slot
// timestamp query pool, and reads the device's timestampPeriod. Fully defensive:
// on any failure it leaves extTimestampPeriod == 0 so the caller skips timing and
// last_gpu_ms stays -1. Runs once (guarded by extGpuTimingTried). Never fails the
// frame — timing is best-effort and orthogonal to rendering.
void ensure_ext_gpu_timing(RiveRenderContext* ctx, VulkanContext* vk)
{
    if (ctx->extGpuTimingTried)
    {
        return;
    }
    ctx->extGpuTimingTried = true;

    // Require reliable timestamps on all graphics/compute queues and a nonzero
    // period (ns per tick). NVIDIA: timestampComputeAndGraphics == true, period 1.
    VkPhysicalDeviceProperties props{};
    vk->GetPhysicalDeviceProperties(ctx->extPhysicalDevice, &props);
    if (props.limits.timestampPeriod <= 0.0f ||
        props.limits.timestampComputeAndGraphics == VK_FALSE)
    {
        return;
    }

    // Resolve the query PFNs via the device loader (itself obtained from the
    // instance loader the caller handed us). These are not in rive's dispatch table.
    auto gdpa = reinterpret_cast<PFN_vkGetDeviceProcAddr>(
        ctx->extGetInstanceProcAddr(ctx->extInstance, "vkGetDeviceProcAddr"));
    if (gdpa == nullptr)
    {
        return;
    }
    ctx->extCreateQueryPool = reinterpret_cast<PFN_vkCreateQueryPool>(
        gdpa(ctx->extDevice, "vkCreateQueryPool"));
    ctx->extDestroyQueryPool = reinterpret_cast<PFN_vkDestroyQueryPool>(
        gdpa(ctx->extDevice, "vkDestroyQueryPool"));
    ctx->extGetQueryPoolResults = reinterpret_cast<PFN_vkGetQueryPoolResults>(
        gdpa(ctx->extDevice, "vkGetQueryPoolResults"));
    ctx->extCmdResetQueryPool = reinterpret_cast<PFN_vkCmdResetQueryPool>(
        gdpa(ctx->extDevice, "vkCmdResetQueryPool"));
    ctx->extCmdWriteTimestamp = reinterpret_cast<PFN_vkCmdWriteTimestamp>(
        gdpa(ctx->extDevice, "vkCmdWriteTimestamp"));
    if (ctx->extCreateQueryPool == nullptr || ctx->extDestroyQueryPool == nullptr ||
        ctx->extGetQueryPoolResults == nullptr ||
        ctx->extCmdResetQueryPool == nullptr || ctx->extCmdWriteTimestamp == nullptr)
    {
        return;
    }

    VkQueryPoolCreateInfo qpInfo{};
    qpInfo.sType = VK_STRUCTURE_TYPE_QUERY_POOL_CREATE_INFO;
    qpInfo.queryType = VK_QUERY_TYPE_TIMESTAMP;
    qpInfo.queryCount = 2;
    if (ctx->extCreateQueryPool(ctx->extDevice, &qpInfo, nullptr, &ctx->extQueryPool) !=
        VK_SUCCESS)
    {
        ctx->extQueryPool = VK_NULL_HANDLE;
        return;
    }
    // Success: timing is now enabled (extTimestampPeriod becomes the gate).
    ctx->extTimestampPeriod = props.limits.timestampPeriod;
}

} // namespace

extern "C" RiveRenderContext* rive_render_context_create_vulkan_external(
    uint64_t instance,
    uint64_t physicalDevice,
    uint64_t device,
    void* getInstanceProcAddr,
    const RiveVulkanFeatures* features,
    int32_t forceAtomic)
{
    if (instance == 0 || physicalDevice == 0 || device == 0 ||
        getInstanceProcAddr == nullptr || features == nullptr)
    {
        set_error("rive_render_context_create_vulkan_external: invalid arguments");
        return nullptr;
    }

    auto ctx = new (std::nothrow) RiveRenderContext();
    if (ctx == nullptr)
    {
        set_error("out of memory allocating RiveRenderContext");
        return nullptr;
    }

    // Mirror the caller's (wgpu-enabled) features into rive's struct field by
    // field — never reinterpret-cast across the ABI boundary.
    rive::gpu::VulkanFeatures vf{};
    vf.apiVersion = features->apiVersion;
    vf.independentBlend = features->independentBlend != 0;
    vf.fillModeNonSolid = features->fillModeNonSolid != 0;
    vf.fragmentStoresAndAtomics = features->fragmentStoresAndAtomics != 0;
    vf.shaderClipDistance = features->shaderClipDistance != 0;
    vf.rasterizationOrderColorAttachmentAccess =
        features->rasterizationOrderColorAttachmentAccess != 0;
    vf.fragmentShaderPixelInterlock = features->fragmentShaderPixelInterlock != 0;
    vf.VK_KHR_portability_subset = features->vkKhrPortabilitySubset != 0;
    vf.textureCompressionBC = features->textureCompressionBC != 0;
    vf.textureCompressionASTC_LDR = features->textureCompressionASTC_LDR != 0;
    vf.textureCompressionETC2 = features->textureCompressionETC2 != 0;

    RenderContextVulkanImpl::ContextOptions ctxOpts;
    ctxOpts.forceAtomicMode = (forceAtomic != 0);

    ctx->renderContext = RenderContextVulkanImpl::MakeContext(
        handle_from_u64<VkInstance>(instance),
        handle_from_u64<VkPhysicalDevice>(physicalDevice),
        handle_from_u64<VkDevice>(device),
        vf,
        reinterpret_cast<PFN_vkGetInstanceProcAddr>(getInstanceProcAddr),
        ctxOpts);
    if (ctx->renderContext == nullptr)
    {
        set_error("RenderContextVulkanImpl::MakeContext (external) failed");
        delete ctx;
        return nullptr;
    }
    ctx->impl = ctx->renderContext->static_impl_cast<RenderContextVulkanImpl>();
    ctx->external = true;
    ctx->extInstance = handle_from_u64<VkInstance>(instance);
    ctx->extPhysicalDevice = handle_from_u64<VkPhysicalDevice>(physicalDevice);
    ctx->extDevice = handle_from_u64<VkDevice>(device);
    ctx->extGetInstanceProcAddr =
        reinterpret_cast<PFN_vkGetInstanceProcAddr>(getInstanceProcAddr);
    return ctx;
}

extern "C" void rive_render_context_set_queue_family(RiveRenderContext* ctx,
                                                     uint32_t queueFamilyIndex)
{
    if (ctx == nullptr)
        return;
    ctx->extQueueFamily = queueFamilyIndex;
}

extern "C" void rive_render_context_set_clockwise(RiveRenderContext* ctx,
                                                  int32_t enabled)
{
    if (ctx == nullptr)
        return;
    ctx->extClockwise = (enabled != 0);
}

extern "C" double rive_render_context_last_gpu_ms(const RiveRenderContext* ctx)
{
    if (ctx == nullptr)
        return -1.0;
    return ctx->extLastGpuMs;
}

extern "C" double rive_render_context_last_flush_us(const RiveRenderContext* ctx)
{
    if (ctx == nullptr)
        return -1.0;
    return ctx->extLastFlushUs;
}

extern "C" double rive_render_context_last_fence_wait_us(const RiveRenderContext* ctx)
{
    if (ctx == nullptr)
        return -1.0;
    return ctx->extLastFenceWaitUs;
}

extern "C" int32_t rive_render_context_supports_raster_ordering(
    const RiveRenderContext* ctx)
{
    if (ctx == nullptr || ctx->renderContext == nullptr)
        return -1;
    return ctx->renderContext->platformFeatures().supportsRasterOrderingMode ? 1 : 0;
}

extern "C" RivePlsMode rive_render_context_pls_mode(const RiveRenderContext* ctx)
{
    if (ctx == nullptr || ctx->renderContext == nullptr)
        return -1;
    // frameInterlockMode() is valid only between beginFrame and flush, so return
    // the value captured at the last beginFrame (extLastInterlockMode) rather than
    // querying live — the Bevy node queries pls_mode() after flush completes.
    return static_cast<RivePlsMode>(ctx->extLastInterlockMode);
}

extern "C" RiveRenderTarget* rive_render_target_wrap_vk_image(
    RiveRenderContext* ctx,
    uint64_t vkImage,
    uint64_t vkImageView,
    uint32_t width,
    uint32_t height,
    uint32_t vkFormat,
    uint32_t vkUsageFlags)
{
    if (ctx == nullptr || ctx->impl == nullptr || !ctx->external || vkImage == 0 ||
        width == 0 || height == 0)
    {
        set_error("rive_render_target_wrap_vk_image: invalid arguments");
        return nullptr;
    }

    auto target = new (std::nothrow) RiveRenderTarget();
    if (target == nullptr)
    {
        set_error("out of memory allocating RiveRenderTarget");
        return nullptr;
    }
    target->external = true;
    target->width = width;
    target->height = height;
    target->extImage = handle_from_u64<VkImage>(vkImage);
    // wgpu just allocated/owns the image; rive has not touched it yet.
    target->lastAccess = ImageAccess{};

    // Build (or adopt) the image view rive samples/renders through.
    if (vkImageView != 0)
    {
        target->extView = handle_from_u64<VkImageView>(vkImageView);
    }
    else
    {
        VkImageViewCreateInfo ivci{};
        ivci.sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO;
        ivci.image = target->extImage;
        ivci.viewType = VK_IMAGE_VIEW_TYPE_2D;
        ivci.format = static_cast<VkFormat>(vkFormat);
        ivci.components = {VK_COMPONENT_SWIZZLE_IDENTITY,
                           VK_COMPONENT_SWIZZLE_IDENTITY,
                           VK_COMPONENT_SWIZZLE_IDENTITY,
                           VK_COMPONENT_SWIZZLE_IDENTITY};
        ivci.subresourceRange.aspectMask = VK_IMAGE_ASPECT_COLOR_BIT;
        ivci.subresourceRange.baseMipLevel = 0;
        ivci.subresourceRange.levelCount = 1;
        ivci.subresourceRange.baseArrayLayer = 0;
        ivci.subresourceRange.layerCount = 1;
        target->ownedView =
            ctx->impl->vulkanContext()->makeExternalImageView(ivci, "rive_ext_target_view");
        if (target->ownedView == nullptr)
        {
            set_error("rive external: makeExternalImageView failed");
            delete target;
            return nullptr;
        }
        target->extView = target->ownedView->vkImageView();
    }

    target->renderTarget = ctx->impl->makeRenderTarget(
        width,
        height,
        static_cast<VkFormat>(vkFormat),
        static_cast<VkImageUsageFlags>(vkUsageFlags));
    if (target->renderTarget == nullptr)
    {
        set_error("RenderContextVulkanImpl::makeRenderTarget (external) failed");
        delete target;
        return nullptr;
    }
    return target;
}

extern "C" void rive_render_target_set_vk_image(RiveRenderTarget* target,
                                                uint64_t vkImage,
                                                uint64_t vkImageView)
{
    if (target == nullptr || !target->external)
        return;
    target->extImage = handle_from_u64<VkImage>(vkImage);
    // A new image is in its initial (undefined) layout from rive's perspective.
    target->lastAccess = ImageAccess{};
    if (vkImageView != 0)
    {
        target->ownedView = nullptr;
        target->extView = handle_from_u64<VkImageView>(vkImageView);
    }
    // If vkImageView==0 the caller keeps the previously-created view; a full
    // recreate (on a genuine resize) goes through wrap_vk_image instead.
}

extern "C" RiveStatus rive_frame_begin_external(RiveRenderContext* ctx,
                                                RiveRenderTarget* target,
                                                float r, float g, float b, float a,
                                                uint64_t currentFrameNumber,
                                                uint64_t safeFrameNumber)
{
    if (ctx == nullptr || ctx->renderContext == nullptr || !ctx->external ||
        target == nullptr || !target->external || target->renderTarget == nullptr)
    {
        set_error("rive_frame_begin_external: invalid arguments");
        return 1;
    }
    if (ctx->currentRenderer != nullptr)
    {
        set_error("rive_frame_begin_external: a frame is already in progress");
        return 1;
    }
    if (currentFrameNumber == 0)
    {
        set_error("rive_frame_begin_external: currentFrameNumber must be nonzero");
        return 1;
    }

    // Bind the wgpu image into rive's render target for this frame, seeding the
    // tracked prior layout (UNDEFINED first frame, SHADER_READ_ONLY after our
    // previous post-flush barrier). rive's flush transitions it to COLOR itself.
    target->renderTarget->setTargetImageView(target->extView,
                                             target->extImage,
                                             target->lastAccess);

    RenderContext::FrameDescriptor frameDescriptor;
    frameDescriptor.renderTargetWidth = target->width;
    frameDescriptor.renderTargetHeight = target->height;
    frameDescriptor.loadAction = rive::gpu::LoadAction::clear;
    // M2.0 perf lever: route the clockwise PLS override into this frame (default
    // false -> rive picks rasterOrdering/atomics as before).
    frameDescriptor.clockwiseFillOverride = ctx->extClockwise;
    frameDescriptor.clearColor =
        rive::colorARGB(to_u8(a), to_u8(r), to_u8(g), to_u8(b));
    ctx->renderContext->beginFrame(frameDescriptor);
    // Capture the interlock/PLS mode now — valid only between beginFrame and
    // flush. The pls_mode getter returns this cached value after the frame.
    ctx->extLastInterlockMode =
        static_cast<int>(ctx->renderContext->frameInterlockMode());

    ctx->currentRenderer =
        new (std::nothrow) rive::RiveRenderer(ctx->renderContext.get());
    if (ctx->currentRenderer == nullptr)
    {
        set_error("out of memory allocating RiveRenderer");
        return 1;
    }
    ctx->currentTarget = target;
    ctx->extCurrentFrameNumber = currentFrameNumber;
    ctx->extSafeFrameNumber = safeFrameNumber;
    return RIVE_OK;
}

extern "C" RiveStatus rive_frame_submit_external(RiveRenderContext* ctx,
                                                 RiveRenderTarget* target,
                                                 uint64_t queue)
{
    if (ctx == nullptr || ctx->renderContext == nullptr || ctx->impl == nullptr ||
        !ctx->external || target == nullptr || !target->external ||
        ctx->currentTarget != target || queue == 0)
    {
        set_error("rive_frame_submit_external: no external frame in progress");
        return 1;
    }

    VulkanContext* vk = ctx->impl->vulkanContext();
    RiveStatus status = RIVE_OK;

    if (!ensure_ext_frame_objects(ctx, vk))
    {
        // error already set
        delete ctx->currentRenderer;
        ctx->currentRenderer = nullptr;
        ctx->currentTarget = nullptr;
        return 1;
    }
    VkCommandBuffer cb = ctx->extCmdBuffer;

    // GPU timing is best-effort (defensive, one-time setup); enabled only if the
    // device + PFNs + query pool are all available.
    ensure_ext_gpu_timing(ctx, vk);
    const bool gpuTiming =
        ctx->extTimestampPeriod > 0.0f && ctx->extQueryPool != VK_NULL_HANDLE;

    // Begin records into the reused command buffer (RESET_COMMAND_BUFFER pool ->
    // Begin implicitly resets it; the previous frame was fence-waited below).
    VkCommandBufferBeginInfo beginInfo{};
    beginInfo.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO;
    beginInfo.flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT;
    if (vk->BeginCommandBuffer(cb, &beginInfo) != VK_SUCCESS)
    {
        set_error("rive external: vkBeginCommandBuffer failed");
        delete ctx->currentRenderer;
        ctx->currentRenderer = nullptr;
        ctx->currentTarget = nullptr;
        return 1;
    }

    // GPU timing: reset the pool (required before reuse) and stamp the start of
    // rive's recorded work (TOP_OF_PIPE).
    if (gpuTiming)
    {
        ctx->extCmdResetQueryPool(cb, ctx->extQueryPool, 0, 2);
        ctx->extCmdWriteTimestamp(cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,
                                  ctx->extQueryPool, 0);
    }

    // rive RECORDS its draws (and its own ->COLOR_ATTACHMENT barrier, via
    // accessTargetImage) into `cb`. It does NOT submit.
    RenderContext::FlushResources flushResources;
    flushResources.renderTarget = target->renderTarget.get();
    flushResources.externalCommandBuffer = reinterpret_cast<void*>(cb);
    flushResources.currentFrameNumber = ctx->extCurrentFrameNumber;
    flushResources.safeFrameNumber = ctx->extSafeFrameNumber;
    // M2a: time rive's CPU-side flush (command-buffer record) in isolation, so the
    // perf collector can attribute the submit wall to flush vs the blocking fence.
    const auto flushT0 = std::chrono::steady_clock::now();
    ctx->renderContext->flush(flushResources);
    ctx->extLastFlushUs = std::chrono::duration<double, std::micro>(
                              std::chrono::steady_clock::now() - flushT0)
                              .count();

    // Post-flush: transition COLOR_ATTACHMENT -> SHADER_READ_ONLY for the wgpu
    // sampling pass, and keep rive's own layout tracker in sync for next frame.
    // (We do NOT record a ->COLOR barrier here; rive's flush already did.)
    ImageAccess readAccess{};
    readAccess.pipelineStages = VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT;
    readAccess.accessMask = VK_ACCESS_SHADER_READ_BIT;
    readAccess.layout = VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL;
    target->lastAccess = vk->simpleImageMemoryBarrier(
        cb, target->renderTarget->targetLastAccess(), readAccess, target->extImage);
    target->renderTarget->updateLastAccess(target->lastAccess);

    // GPU timing: stamp the end of rive's recorded work (after its draws + the
    // post-flush barrier) at BOTTOM_OF_PIPE. Slot 0 (start) .. slot 1 (end) span
    // rive's whole command buffer.
    if (gpuTiming)
    {
        ctx->extCmdWriteTimestamp(cb, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT,
                                  ctx->extQueryPool, 1);
    }

    if (vk->EndCommandBuffer(cb) != VK_SUCCESS)
    {
        set_error("rive external: vkEndCommandBuffer failed");
        status = 1;
    }
    else
    {
        // Reset the shim-internal fence, submit OUT-OF-BAND to the wgpu queue,
        // then BLOCK on the fence so the shared image is fully written +
        // transitioned to SHADER_READ_ONLY before this returns (M1b is
        // correctness-first; transition_resources + pipelined fences are M2).
        vk->ResetFences(ctx->extDevice, 1, &ctx->extFence);
        VkSubmitInfo submitInfo{};
        submitInfo.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO;
        submitInfo.commandBufferCount = 1;
        submitInfo.pCommandBuffers = &cb;
        if (vk->QueueSubmit(handle_from_u64<VkQueue>(queue), 1, &submitInfo,
                            ctx->extFence) != VK_SUCCESS)
        {
            set_error("rive external: vkQueueSubmit failed");
            status = 1;
        }
        else
        {
            // M2a: time the blocking fence wait in isolation — this is the per-frame
            // stall the non-blocking-sync rework removes (Step 0 fence-vs-flush
            // split). Measured around the exact WaitForFences call.
            const auto fenceT0 = std::chrono::steady_clock::now();
            const VkResult waitRes =
                vk->WaitForFences(ctx->extDevice, 1, &ctx->extFence, VK_TRUE, UINT64_MAX);
            ctx->extLastFenceWaitUs = std::chrono::duration<double, std::micro>(
                                          std::chrono::steady_clock::now() - fenceT0)
                                          .count();
            if (waitRes != VK_SUCCESS)
            {
                set_error("rive external: vkWaitForFences failed");
                status = 1;
            }
            else if (gpuTiming)
            {
                // The fence is signaled, so rive's GPU work — and both timestamp
                // writes — have completed; the results are available now. Read the
                // two ticks and convert to milliseconds (period is ns/tick).
                // Best-effort: any failure or a non-increasing pair reports -1.
                uint64_t ts[2] = {0, 0};
                if (ctx->extGetQueryPoolResults(
                        ctx->extDevice, ctx->extQueryPool, 0, 2, sizeof(ts), ts,
                        sizeof(uint64_t),
                        VK_QUERY_RESULT_64_BIT | VK_QUERY_RESULT_WAIT_BIT) == VK_SUCCESS &&
                    ts[1] > ts[0])
                {
                    ctx->extLastGpuMs = static_cast<double>(ts[1] - ts[0]) *
                                        static_cast<double>(ctx->extTimestampPeriod) /
                                        1.0e6;
                }
                else
                {
                    ctx->extLastGpuMs = -1.0;
                }
            }
        }
    }

    delete ctx->currentRenderer;
    ctx->currentRenderer = nullptr;
    ctx->currentTarget = nullptr;
    return status;
}

// M2a non-blocking sync: record rive's frame into a CALLER-PROVIDED, already-open
// command buffer (wgpu's own, obtained via as_hal_mut().raw_handle()), then return
// WITHOUT submitting or fencing. rive's draws ride wgpu's single per-frame submit,
// GPU-ordered before the later wgpu pass that samples the image — no CPU stall,
// no separate VkQueue submit, no vkWaitForFences. Contrast rive_frame_submit_external
// (the M1b blocking path), which owns its command buffer + fence and blocks.
//
// Correctness contract upheld by the caller (the bevy-rive node):
//   * `cmdBuffer` is wgpu's open primary command buffer for THIS frame; we never
//     Begin/End/submit it (wgpu does, at finish()).
//   * The post-flush barrier leaves the image in SHADER_READ_ONLY_OPTIMAL — which
//     equals wgpu's tracked RESOURCE layout — so wgpu emits no destructive barrier
//     when it samples (steady state); rive's barrier provides write->read visibility.
//   * safeFrameNumber (seeded at begin) must trail currentFrameNumber by rive's ring
//     size, since there is no fence: a frame's resources are only safe to recycle
//     once its GPU work has completed, which is bounded by frames-in-flight.
extern "C" RiveStatus rive_frame_record_external(RiveRenderContext* ctx,
                                                 RiveRenderTarget* target,
                                                 uint64_t cmdBuffer)
{
    if (ctx == nullptr || ctx->renderContext == nullptr || ctx->impl == nullptr ||
        !ctx->external || target == nullptr || !target->external ||
        ctx->currentTarget != target || cmdBuffer == 0)
    {
        set_error("rive_frame_record_external: no external frame in progress");
        return 1;
    }

    VulkanContext* vk = ctx->impl->vulkanContext();
    VkCommandBuffer cb = handle_from_u64<VkCommandBuffer>(cmdBuffer);

    // rive RECORDS its draws (and its own ->COLOR_ATTACHMENT barrier, via
    // accessTargetImage) into wgpu's open buffer. It does NOT submit.
    RenderContext::FlushResources flushResources;
    flushResources.renderTarget = target->renderTarget.get();
    flushResources.externalCommandBuffer = reinterpret_cast<void*>(cb);
    flushResources.currentFrameNumber = ctx->extCurrentFrameNumber;
    flushResources.safeFrameNumber = ctx->extSafeFrameNumber;
    const auto flushT0 = std::chrono::steady_clock::now();
    ctx->renderContext->flush(flushResources);
    ctx->extLastFlushUs = std::chrono::duration<double, std::micro>(
                              std::chrono::steady_clock::now() - flushT0)
                              .count();
    // No blocking fence in this path — that is the whole point of M2a.
    ctx->extLastFenceWaitUs = 0.0;
    // GPU timing needs a completion signal we don't have here (wgpu submits this
    // buffer asynchronously); report unavailable. rive's recorded commands are
    // identical to the blocking path, so Step 0's GPU baseline still applies.
    ctx->extLastGpuMs = -1.0;

    // Post-flush: transition COLOR_ATTACHMENT -> SHADER_READ_ONLY for wgpu's
    // sampling pass, recorded into wgpu's buffer; keep rive's layout tracker in
    // sync so the next frame's begin seeds the right prior layout.
    ImageAccess readAccess{};
    readAccess.pipelineStages = VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT;
    readAccess.accessMask = VK_ACCESS_SHADER_READ_BIT;
    readAccess.layout = VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL;
    target->lastAccess = vk->simpleImageMemoryBarrier(
        cb, target->renderTarget->targetLastAccess(), readAccess, target->extImage);
    target->renderTarget->updateLastAccess(target->lastAccess);

    delete ctx->currentRenderer;
    ctx->currentRenderer = nullptr;
    ctx->currentTarget = nullptr;
    return RIVE_OK;
}

extern "C" uint64_t rive_render_target_vk_image(const RiveRenderTarget* target)
{
    if (target == nullptr || !target->external)
        return 0;
    return handle_to_u64(target->extImage);
}

extern "C" uint64_t rive_render_target_vk_image_view(const RiveRenderTarget* target)
{
    if (target == nullptr || !target->external)
        return 0;
    return handle_to_u64(target->extView);
}

// ---------------------------------------------------------------------------
// Backend-tagged d3d12 / metal siblings — DESIGN ONLY (stubbed in M1b).
// ---------------------------------------------------------------------------

extern "C" RiveRenderContext* rive_render_context_create_d3d12_external(void*, void*, int32_t)
{
    set_error("rive d3d12 external context is not implemented in M1b (Vulkan only)");
    return nullptr;
}

extern "C" RiveRenderTarget* rive_render_target_wrap_d3d12_resource(
    RiveRenderContext*, void*, uint32_t, uint32_t, uint32_t)
{
    set_error("rive d3d12 external target is not implemented in M1b (Vulkan only)");
    return nullptr;
}

extern "C" RiveStatus rive_frame_submit_external_d3d12(
    RiveRenderContext*, RiveRenderTarget*, void*, void*, uint64_t)
{
    set_error("rive d3d12 external submit is not implemented in M1b (Vulkan only)");
    return 1;
}

extern "C" RiveRenderContext* rive_render_context_create_metal_external(void*, void*)
{
    set_error("rive metal external context is not implemented in M1b (Vulkan only)");
    return nullptr;
}

extern "C" RiveRenderTarget* rive_render_target_wrap_metal_texture(
    RiveRenderContext*, void*, uint32_t, uint32_t, uint32_t)
{
    set_error("rive metal external target is not implemented in M1b (Vulkan only)");
    return nullptr;
}

extern "C" RiveStatus rive_frame_submit_external_metal(RiveRenderContext*,
                                                       RiveRenderTarget*,
                                                       void*)
{
    set_error("rive metal external submit is not implemented in M1b (Vulkan only)");
    return 1;
}
