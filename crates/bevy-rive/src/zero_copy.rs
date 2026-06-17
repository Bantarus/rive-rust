//! M1b — the **zero-copy Vulkan tier** for `bevy-rive`.
//!
//! Where the M1a floor (see the crate root) renders rive to its own offscreen
//! device, reads the pixels back to the CPU, and copies them into a Bevy
//! [`Image`] each frame, M1b shares **one** GPU device with wgpu and renders the
//! `.riv` *directly into a wgpu-allocated `VkImage`* — no per-frame CPU readback.
//!
//! This whole module is gated behind the `zero_copy` cargo feature. The frozen
//! M1a ECS API ([`RiveFile`], [`RiveAnimation`], [`RiveTarget`], the selectors,
//! and the `RiveTarget.image` `Handle<Image>` + upright-orientation seam) is
//! **unchanged** — M1b swaps only the fill mechanism, and the user still
//! displays the image with a `Sprite`, exactly as in M1a.
//!
//! # Architecture (see `docs/design/M1B_DESIGN_SPEC.md`)
//!
//! 1. **Device sharing.** [`install_interlock_device_callback`] inserts a Bevy
//!    `RawVulkanInitSettings` callback (before `DefaultPlugins`) that appends
//!    `VK_EXT_fragment_shader_interlock` (or the AMD raster-order ext) to the
//!    device Bevy creates, so rive gets its clean raster-order PLS path. Bevy
//!    keeps owning the wgpu device.
//! 2. **Handle extraction.** A `RenderStartup`-ish system reads the raw
//!    `VkInstance/VkPhysicalDevice/VkDevice/VkQueue/queueFamily/loader` from
//!    Bevy's `RenderDevice`/`RenderAdapter`/`RenderInstance` via the guard-form
//!    `as_hal`, mirrors the *actually-enabled* device features into a
//!    [`rive_renderer::VulkanFeatures`], and stores them in [`RiveSharedHandles`].
//! 3. **Render-world native state.** rive's `!Send` objects (the external
//!    `Context`, per-entity artboard / state-machine / wrapped target, and the
//!    shared wgpu textures) live in [`RiveRenderState`] — a render-world resource
//!    whose `!Send` interior is upheld by a strict single-thread invariant
//!    (touched only inside the render-graph node / extract, which run serialized
//!    on the render thread). The wrapper's handles are non-atomic `Rc`-refcounted,
//!    so a cross-thread *drop* would be UNSOUND — which is exactly why this tier
//!    holds them as a `NonSend` resource with pipelined rendering disabled. The
//!    single-thread invariant (not atomics) is what makes them safe here; do not
//!    re-enable pipelining without first switching the wrapper to atomic refcounts.
//! 4. **Per-frame.** An `Extract` system copies `Send` per-entity data (the
//!    display `Handle<Image>`, the `.riv` bytes, size, and `dt·speed`) into the
//!    render world. The [`RiveFillNode`] render-graph node (ordered before
//!    `StartMainPass` in the [`RiveGraphAnchor`]-selected sub-graph) lazily builds rive's context + per-entity
//!    instances, advances each state machine, renders it into the **shared**
//!    `Rgba8Unorm` texture out-of-band (rive submits its own command buffer; the
//!    shim fences), then copies that texture into the **display**
//!    `Rgba8UnormSrgb` `Image` the `Sprite` samples.
//!
//! # Display — the un-premultiply pass (design spec §7 Option B, IMPLEMENTED)
//!
//! rive renders **premultiplied, sRGB-encoded** bytes into the internal shared
//! `Rgba8Unorm` texture. A fullscreen un-premultiply + sRGB-decode pass — recorded
//! inside [`RiveFillNode`] (after rive's draws, before `StartMainPass`) by the
//! `RiveBlitPipeline` (`UNPREMULT_WGSL`) — un-premultiplies in ENCODED space,
//! sRGB-decodes, and writes the **straight-alpha** result into the `Rgba8UnormSrgb`
//! display image that becomes `RiveTarget.image`. Correct for **both** opaque
//! (premultiplied == straight; matches the M0/M1.0 references exactly) and
//! **transparent** content (≤1–2 LSB on AA edges, no fringe). Both tiers therefore
//! end at the same **straight-alpha `Rgba8UnormSrgb`** seam — a `Sprite`, or a 3D
//! `StandardMaterial` with `AlphaMode::Blend`, composites `RiveTarget.image`
//! directly (NOT `AlphaMode::Premultiplied` — the image is straight, not premultiplied).

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_void, CStr};

use ash::vk;
use ash::vk::Handle as _; // brings `as_raw()` on Vulkan handle types into scope
use bevy::asset::RenderAssetUsages;
use bevy::prelude::*;
use bevy::render::render_asset::RenderAssets;
use bevy::render::render_graph::{
    Node, NodeRunError, RenderGraphContext, RenderGraphExt, RenderLabel,
};
use bevy::render::render_resource::{
    Extent3d, Texture, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages,
    TextureView,
};
use bevy::render::renderer::{RenderContext, RenderDevice, RenderQueue};
use bevy::render::texture::GpuImage;
use bevy::render::{Extract, RenderApp};

use bevy::core_pipeline::core_2d::graph::{Core2d, Node2d};
use bevy::core_pipeline::core_3d::graph::{Core3d, Node3d};
use bevy::render::renderer::raw_vulkan_init::{AdditionalVulkanFeatures, RawVulkanInitSettings};
use bevy::render::renderer::{RenderAdapter, RenderInstance};
use bevy::render::view::ExtractedWindows;
use bevy::window::PresentMode;

use rive_renderer::{
    Artboard, Context, ExternalFrameRecord, ExternalFrameSubmit, FitAlign, RenderTarget,
    StateMachine, VulkanFeatures,
};

use crate::{
    RiveActive, RiveAnimation, RiveAssets, RiveFile, RiveFit, RivePlugin, RivePointer, RiveSurface,
    RiveTarget, RiveText, RiveValue, RiveViewModel,
};

type Vk = wgpu_hal::vulkan::Api;

/// `VK_FORMAT_R8G8B8A8_UNORM` (== 37) — the rive shared target's `VkFormat`.
const VK_FORMAT_R8G8B8A8_UNORM: u32 = 37;

/// `VkImageUsageFlags` matching the shared texture's wgpu usages:
/// `COLOR_ATTACHMENT (0x10) | SAMPLED (0x4) | TRANSFER_DST (0x2) | TRANSFER_SRC (0x1)`.
/// rive's `RenderTargetVulkan` ctor requires `INPUT_ATTACHMENT` *or* both
/// `TRANSFER_SRC+DST`; we provide the transfer pair so its blended-content
/// offscreen blit-back lands in our image.
const RIVE_TARGET_VK_USAGE: u32 = 0x10 | 0x04 | 0x02 | 0x01;

// The rive clear color is shared with the M1a path via `crate::rive_clear_rgba()`
// (honors the `RIVE_CLEAR_ALPHA` test knob), so both tiers clear identically.

/// The shared (internal) texture rive renders into: **linear** `Rgba8Unorm`, so
/// the WGSL/sampler sees rive's raw sRGB-encoded premultiplied bytes verbatim.
const SHARED_FORMAT: TextureFormat = TextureFormat::Rgba8Unorm;
/// M-SCALE Phase 3 — per-LOD atlas tile sizes (px), ascending. A face's bucket is the
/// smallest tile ≥ its `max(width, height)`; larger faces clamp to the last bucket (they
/// render at reduced resolution — the intended LOD tradeoff). 512 matches the M1a/example
/// target, so the 512 bucket reproduces the Phase-2 single-page record cost exactly; the
/// 128/256 buckets give distant/small faces a cheaper tile (less tessellation + far less
/// coverage VRAM, since the PLS coverage backing scales with PAGE area, not fill).
const ATLAS_BUCKETS: [u32; 3] = [128, 256, 512];
/// Tiles per side of one atlas page → `ATLAS_TILES_PER_PAGE` tiles/page. 16 ⇒ 256
/// tiles/page: the count validated in Phase 2 (one octopus flush) and comfortably under
/// rive's per-flush caps (contours ≤ 65535, draw-passes ≤ 32767 in atomic mode —
/// `render_context.cpp:497-522`; exceeding them is a correctness-preserving auto-split,
/// not a crash). A FULL page is VRAM-efficient, so growth adds a SIBLING page rather than
/// enlarging one (the C1 budget). Largest page = 512×16 = 8192px (within the 16384 cap).
const ATLAS_TILES_PER_SIDE: u32 = 16;
/// Tiles in one page — a fixed `ATLAS_TILES_PER_SIDE`² grid, so slot alloc is an O(1)
/// bump cursor + free-list (no shelf packer, no fragmentation, no repack-invalidates-all).
const ATLAS_TILES_PER_PAGE: u32 = ATLAS_TILES_PER_SIDE * ATLAS_TILES_PER_SIDE;
/// C2 writer-side gutter (px inset per tile side). rive's `clipRect` AA writes fractional
/// coverage ~0.5px OUTSIDE the nominal rect (`common.glsl`), so edge-to-edge tiles corrupt
/// each other's boundary. Each tile's drawn viewport AND its `uv_rect` are inset by this,
/// leaving a transparent ≥`2·GUTTER` band between neighbours. Page dimensions are unchanged.
const ATLAS_GUTTER_PX: u32 = 2;

/// The display texture behind `RiveTarget.image`: `Rgba8UnormSrgb` straight-alpha,
/// **identical to the M1a seam**, so the user's `Sprite` path is unchanged.
const DISPLAY_FORMAT: TextureFormat = TextureFormat::Rgba8UnormSrgb;

/// rive's transient-resource ring depth (`gpu::kBufferRingSize`), used as the
/// **fallback** recycle watermark. rive's `acquire()` reuses a pooled, host-mapped
/// buffer iff its `lastFrameNumber <= safeFrameNumber`, then a CPU memcpy rewrites
/// it — so `safe_frame` must never name a frame whose GPU work is still in flight.
///
/// M2b's default is an EXACT watermark: the node reads our own Vulkan timeline
/// semaphore (signalled with the frame number on each of wgpu's per-frame submits)
/// to learn the highest frame the GPU has actually finished — with no fixed
/// assumption about frames-in-flight, so any present mode / frame latency is sound
/// (see [`RiveSharedHandles::frame_sync_sema`]).
///
/// This constant is the fallback used only when timeline semaphores are unavailable:
/// `safe_frame = current - RIVE_RING_SIZE`, correct **only while frames-in-flight ≤
/// RIVE_RING_SIZE**. Bevy's default surface (Fifo / AutoVsync,
/// `desired_maximum_frame_latency` 2 → a 3-image swapchain) caps the CPU at ~3 frames
/// ahead, matching the ring; non-Fifo present modes (Immediate / Mailbox /
/// AutoNoVsync) or a higher latency break it, so in the fallback the node emits a
/// one-shot warning and `RIVE_BLOCKING=1` is the safe escape hatch. Must match rive's
/// `kBufferRingSize`.
const RIVE_RING_SIZE: u64 = 3;

// ===========================================================================
// Plugin.
// ===========================================================================

/// Which Bevy render sub-graph(s) the zero-copy [`RiveFillNode`] is anchored in.
///
/// Bevy runs a camera sub-graph **only when a camera targets it**, so the anchor MUST
/// match the consuming scene's cameras. A pure-3D scene (a `Camera3d`, **no** `Camera2d`
/// — a `bevy_ui` HUD does *not* imply a `Camera2d`) needs [`Core3d`](Self::Core3d), or
/// the fill node never executes and the texture is frozen on frame 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RiveGraphAnchor {
    /// Core2d only — fill before `Node2d::StartMainPass` (2D `Sprite`/`Material2d` scenes).
    Core2d,
    /// Core3d only — fill before `Node3d::StartMainPass` (pure-3D `StandardMaterial` scenes).
    Core3d,
    /// **Both** (default) — zero-config: the fill runs in whichever sub-graph has a
    /// camera, and is simply not executed in a sub-graph with no matching camera (so a
    /// pure-2D *or* pure-3D scene each pays for exactly one fill). In a scene with BOTH a
    /// `Camera2d` and a `Camera3d` the fill runs once per sub-graph — i.e. twice —
    /// which is harmless (idempotent re-render), just redundant GPU work that frame.
    #[default]
    Both,
}

/// The M1b zero-copy plugin. Registers the `.riv` asset + loader (the shared
/// [`RivePlugin`] machinery is *not* reused — see below), the main-world display
/// allocation system, the render-world extract + handle systems, and the
/// [`RiveFillNode`] render-graph node — anchored per [`RiveZeroCopyPlugin::anchor`].
///
/// Wiring (see the `sprite_riv_zerocopy` example):
/// ```ignore
/// let mut app = App::new();
/// bevy_rive::install_interlock_device_callback(&mut app); // BEFORE DefaultPlugins
/// app.add_plugins(DefaultPlugins);
/// app.add_plugins(RiveZeroCopyPlugin::default());         // INSTEAD of RivePlugin
/// // pure-3D consumer (Camera3d, no Camera2d):
/// // app.add_plugins(RiveZeroCopyPlugin::anchored(RiveGraphAnchor::Core3d));
/// ```
///
/// `RiveZeroCopyPlugin` registers the asset + loader itself (so it composes
/// without the M1a CPU systems double-driving the same entities). It does *not*
/// add the M1a `NonSend` systems; M1b entities are driven entirely in the render
/// world.
#[derive(Debug, Clone, Default)]
pub struct RiveZeroCopyPlugin {
    /// Which render graph(s) to anchor the fill node in. Defaults to
    /// [`RiveGraphAnchor::Both`] (works with a 2D or 3D camera out of the box).
    pub anchor: RiveGraphAnchor,
}

impl RiveZeroCopyPlugin {
    /// A plugin anchored in the given sub-graph(s) — e.g.
    /// `RiveZeroCopyPlugin::anchored(RiveGraphAnchor::Core3d)` for a pure-3D consumer.
    #[must_use]
    pub fn anchored(anchor: RiveGraphAnchor) -> Self {
        Self { anchor }
    }
}

impl Plugin for RiveZeroCopyPlugin {
    fn build(&self, app: &mut App) {
        // This tier holds rive's `!Send` objects as a NonSend render-world resource,
        // which is only sound if the render world runs on the main thread — i.e.
        // pipelined rendering must be DISABLED. We cannot remove an already-added
        // plugin from here, so surface a loud error if it is still present.
        if app.is_plugin_added::<bevy::render::pipelined_rendering::PipelinedRenderingPlugin>() {
            error!(
                "rive zero-copy: PipelinedRenderingPlugin is ENABLED — this tier requires it \
                 disabled (it owns rive's !Send handles as a main-thread NonSend resource). \
                 Build DefaultPlugins with `.disable::<PipelinedRenderingPlugin>()`."
            );
        }

        // Asset + loader (reuse the frozen M1a types via a tiny private plugin so
        // the `.riv` AssetLoader is registered exactly once and identically).
        RivePlugin::register_asset(app);

        // Main world: allocate the per-face display Image (the frozen seam) once the
        // .riv has loaded, AND assign atlas slots + write `RiveSurface` for opt-in
        // atlas faces (M-SCALE) — so a face's `image` + `uv_rect` land together, the
        // same frame, before any consumer reads them.
        app.init_resource::<RiveAtlasState>()
            .add_systems(Update, (allocate_display_images, allocate_atlas_slots))
            // Audio: apply the optional `RiveAudio` resource (master volume / mute)
            // on change — audio plays automatically during the node's advance.
            .add_systems(Update, crate::audio::apply_rive_audio)
            // M-DATA / M-TEXT: stage view-model + text-run writes in `Last` (after
            // gameplay, before extract), so the read-only extract can ferry them.
            .add_systems(Last, (stage_vm_writes, stage_text_writes));

        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            error!("rive zero-copy: RenderApp missing (enable bevy_render)");
            return;
        };
        render_app
            .init_resource::<ExtractedRives>()
            .add_systems(ExtractSchedule, extract_rive_instances)
            .add_systems(
                bevy::render::Render,
                extract_shared_handles_once.in_set(bevy::render::RenderSystems::Prepare),
            );
        // RiveGpu holds rive's `!Send` handles → a NonSend render-world resource
        // (NOT a `Resource`, which would require `Send + Sync`). Sound because this
        // tier disables pipelined rendering, so the render world runs on the main
        // thread — no cross-thread move or drop, hence no `unsafe Send` needed.
        render_app
            .world_mut()
            .init_non_send_resource::<RiveRenderState>();

        // The fill node, ordered before the main pass that SAMPLES the rive texture, in
        // each sub-graph the consumer selected (`StartMainPass` precedes the opaque +
        // transparent passes in both Core2d sprites and Core3d StandardMaterial). A
        // sub-graph runs only when a camera targets it, so anchoring in the wrong graph
        // leaves the fill dead — hence `anchor` must match the scene's cameras. The same
        // node type + label in two distinct sub-graphs are two independent node instances.
        if matches!(self.anchor, RiveGraphAnchor::Core2d | RiveGraphAnchor::Both) {
            render_app
                .add_render_graph_node::<RiveFillNode>(Core2d, RiveFillLabel)
                .add_render_graph_edges(Core2d, (RiveFillLabel, Node2d::StartMainPass));
        }
        if matches!(self.anchor, RiveGraphAnchor::Core3d | RiveGraphAnchor::Both) {
            render_app
                .add_render_graph_node::<RiveFillNode>(Core3d, RiveFillLabel)
                .add_render_graph_edges(Core3d, (RiveFillLabel, Node3d::StartMainPass));
        }
    }
}

// ===========================================================================
// Device sharing: the interlock callback (runs inside Bevy's device creation).
// ===========================================================================

/// Private marker recorded into `AdditionalVulkanFeatures` when we successfully
/// injected an interlock extension, so a later system can log the tier.
struct RiveInterlock;

/// Installs a Vulkan device-creation callback that enables rive's interlock
/// extension on the device **Bevy** creates. Call this on the `App` **before**
/// `add_plugins(DefaultPlugins)` (Bevy reads the `RawVulkanInitSettings` resource
/// during `RenderPlugin::build`).
///
/// Requires the `zero_copy` feature (which enables `bevy/raw_vulkan_init`). If
/// neither interlock extension is present on the physical device, no extension is
/// added and rive falls back to its atomic PLS path (still correct).
pub fn install_interlock_device_callback(app: &mut App) {
    let mut settings = RawVulkanInitSettings::default();
    // SAFETY: the callback only *adds* an extension after verifying the physical
    // device advertises it, and chains a feature struct that outlives
    // `vkCreateDevice` (leaked). It never removes features or requests anything
    // unsupported. This is the documented contract of `add_create_device_callback`.
    unsafe {
        settings.add_create_device_callback(
            |args: &mut wgpu::hal::vulkan::CreateDeviceCallbackArgs<'_, '_, '_>,
             adapter: &wgpu::hal::vulkan::Adapter,
             feats: &mut AdditionalVulkanFeatures| {
                let phys = adapter.raw_physical_device();
                let instance = adapter.shared_instance().raw_instance();
                // `phys`/`instance` are live for the callback's duration; the
                // enumerate + CStr::from_ptr calls are covered by the outer
                // `unsafe` block this closure is lexically nested in.
                let props = instance
                    .enumerate_device_extension_properties(phys)
                    .unwrap_or_default();
                let has = |name: &CStr| {
                    props
                        .iter()
                        .any(|p| CStr::from_ptr(p.extension_name.as_ptr()) == name)
                };

                let pixel = has(ash::ext::fragment_shader_interlock::NAME);
                let raster = has(ash::ext::rasterization_order_attachment_access::NAME);

                // Enable BOTH interlock paths the device advertises — NOT either/or.
                // rive's raster-ordering mode keys off
                // `rasterizationOrderColorAttachmentAccess`; `fragmentShaderPixelInterlock`
                // feeds its (lower-priority) clockwise mode. This was previously an
                // `if pixel else if raster`, so on a device with both (NVIDIA) the
                // raster-order extension was NEVER enabled — the root cause of the
                // observed Atomics PLS mode. rive prefers raster-ordering when present.
                if raster {
                    args.extensions
                        .push(ash::ext::rasterization_order_attachment_access::NAME);
                    let f = Box::leak(Box::new(
                        vk::PhysicalDeviceRasterizationOrderAttachmentAccessFeaturesEXT::default()
                            .rasterization_order_color_attachment_access(true),
                    ));
                    let info = core::mem::take(args.create_info);
                    *args.create_info = info.push_next(f);
                }
                if pixel {
                    args.extensions
                        .push(ash::ext::fragment_shader_interlock::NAME);
                    let f = Box::leak(Box::new(
                        vk::PhysicalDeviceFragmentShaderInterlockFeaturesEXT::default()
                            .fragment_shader_pixel_interlock(true),
                    ));
                    let info = core::mem::take(args.create_info);
                    *args.create_info = info.push_next(f);
                }
                if pixel || raster {
                    feats.insert::<RiveInterlock>();
                }
                // `fragmentStoresAndAtomics` / `fillModeNonSolid` are part of the
                // core VkPhysicalDeviceFeatures wgpu already requests — do not
                // duplicate here; we read the enabled set back at extraction.

                // EVIDENCE (PLS feature-survival experiment): walk + log the FINAL
                // pNext chain and the interlock extensions we hand wgpu. If the mode
                // is still not RasterOrdering, this distinguishes "wgpu dropped our
                // extension/feature during its VkDeviceCreateInfo rebuild" from
                // "enabled, but rive chose another mode". The raw walk is covered by
                // the enclosing `unsafe` block (same as the enumerate above).
                let mut chain = Vec::new();
                let mut p = args.create_info.p_next;
                while !p.is_null() {
                    let base = p as *const vk::BaseOutStructure;
                    chain.push((*base).s_type);
                    p = (*base).p_next as *const c_void;
                }
                info!(
                    "rive zero-copy: device-create callback pushed interlock exts \
                     (raster={raster}, pixel={pixel}); final pNext sType chain wgpu \
                     will pass to vkCreateDevice = {chain:?}"
                );
            },
        );
    }
    app.insert_resource(settings);
}

// ===========================================================================
// Render-world resources.
// ===========================================================================

/// The shared raw Vulkan handles extracted from Bevy's wgpu device, plus the
/// `VulkanFeatures` rive must mirror. All fields are plain `Copy`/`Send` data
/// (handles as integers); the `!Send` rive objects live in [`RiveRenderState`].
#[derive(Resource, Clone)]
struct RiveSharedHandles {
    instance: u64,
    physical_device: u64,
    device: u64,
    queue: u64,
    queue_family_index: u32,
    /// `PFN_vkGetInstanceProcAddr` as a raw pointer value (stored as `usize` so
    /// the resource is `Send`; cast back to `*mut c_void` at use).
    get_instance_proc_addr: usize,
    features: VulkanFeatures,
    /// Whether an interlock extension was enabled (diagnostics / expected tier).
    interlock: bool,
    /// Force rive's atomic PLS path (`RIVE_FORCE_ATOMIC` env): clears the interlock
    /// feature flags so rive never records interlock commands. An A/B / escape-hatch
    /// knob — to force atomics on interlock-capable HW, or for a hypothetical device
    /// that *advertises* interlock but cannot execute it. (WSL2's Mesa Dozen does NOT
    /// advertise `VK_EXT_fragment_shader_interlock` — pixel=raster=false there — so it
    /// already selects atomics via the capability gate without this flag.)
    force_atomic: bool,
    /// Whether to request rive's per-frame `clockwiseFillOverride` (the clockwise PLS
    /// path). M2c default = `pixel && !raster` (capability-gated: on wherever pixel
    /// interlock is available and raster-order is not), because clockwise is CPU-cheaper
    /// AND throughput-positive there with ≤1 LSB output. Overridable: `RIVE_CLOCKWISE`
    /// forces it on, `RIVE_NO_CLOCKWISE` forces atomics (the A/B baseline).
    clockwise: bool,
    /// M2.0 perf instrumentation (`RIVE_PERF` env): collect per-frame CPU/GPU
    /// timings and log a median+percentile summary after `perf_target` frames.
    perf_enabled: bool,
    /// Post-warmup frame count to summarize over (`RIVE_PERF_FRAMES`, default 300).
    perf_target: u32,
    /// M2a fallback (`RIVE_BLOCKING` env): use the M1b blocking out-of-band submit +
    /// fence instead of the default non-blocking record-into-wgpu path. Kept as a
    /// selectable fallback and an A/B baseline measurable on a single build.
    blocking_submit: bool,
    /// M2b: handle (as `u64`) of our own Vulkan **timeline** semaphore, or `0` if
    /// unavailable. When nonzero, the node signals it with the frame number on each
    /// of wgpu's per-frame submits and reads its counter as the EXACT, non-blocking
    /// GPU-completion watermark for rive's resource recycling — which removes the M2a
    /// "≤ ring frames in flight" precondition entirely. `0` (the blocking path, or a
    /// device without timeline semaphores) falls back to `frame - RIVE_RING_SIZE`.
    ///
    /// Created once in `extract_shared_handles_once`; its lifetime is owned by
    /// [`RiveFrameSync`], which destroys it on teardown (see that type).
    frame_sync_sema: u64,
}

/// Owns the M2b timeline semaphore and destroys it when the render world tears down —
/// fixing `VUID-vkDestroyDevice-device-05137` (the validation gate caught the semaphore
/// outliving the `VkDevice`). It retains a `wgpu::Device` **clone**, which keeps the
/// underlying `VkDevice` alive across destruction regardless of render-world resource drop
/// order, so this is order-independent and is NOT the M1b drop-time hazard (that was the
/// `!Send` rive `Rc` state under pipelining; this is a plain `Send` wgpu handle, with
/// pipelining off).
#[derive(Resource)]
struct RiveFrameSync {
    sema: u64,
    device: wgpu::Device,
}

impl Drop for RiveFrameSync {
    fn drop(&mut self) {
        if self.sema == 0 {
            return;
        }
        // SAFETY: our retained `wgpu::Device` clone keeps the `VkDevice` alive across this
        // call (it cannot be destroyed until this clone drops, which is after this body).
        // `device_wait_idle` first so no in-flight submit still references the semaphore,
        // then destroy it — VUID-vkDestroySemaphore + VUID-vkDestroyDevice clean.
        unsafe {
            if let Some(d) = self.device.as_hal::<Vk>() {
                let raw = d.raw_device();
                let _ = raw.device_wait_idle();
                raw.destroy_semaphore(vk::Semaphore::from_raw(self.sema), None);
            }
        }
    }
}

/// Per-entity native render state (one rive instance bound to one shared texture).
struct RiveInstance {
    artboard: Artboard,
    state_machine: StateMachine,
    /// rive's render target wrapping `shared_tex`'s `VkImage` (zero copy).
    target: RenderTarget,
    /// The wgpu-owned shared color texture rive renders into. Held so its
    /// `VkImage` stays alive while rive references it.
    #[expect(
        dead_code,
        reason = "ownership anchor: keeps the VkImage alive for rive"
    )]
    shared_tex: Texture,
    /// The display `Image` the `Sprite` samples (un-premult pass destination).
    display: Handle<Image>,
    /// A sampled view of `shared_tex` (the un-premult pass's source binding).
    /// Bevy's `TextureView` newtype; `Deref`s to `wgpu::TextureView`.
    shared_view: TextureView,
    /// Cached un-premult-pass bind group (sole input = the stable `shared_view`).
    /// Built once on first render and reused — the shared texture is zero-copy and
    /// never changes, so this removes a per-frame `create_bind_group` per instance.
    bind_group: Option<wgpu::BindGroup>,
    /// Pointer edge-tracking (the zero_copy analogue of the floor `RiveInstance`'s
    /// fields): emit `pointer_down`/`pointer_up` on button edges and a single
    /// `pointer_exit` when the cursor leaves. Lives on the instance (not the
    /// component) because edges are per-native-state. The atlas path has its own
    /// copy on [`AtlasInstance`] with tile-aware inversion (see the atlas loop).
    last_pointer_down: bool,
    last_pointer_present: bool,
}

/// One atlas PAGE (M-SCALE): a shared `Rgba8Unorm` wgpu texture (`page_px`²) that rive
/// renders a page's worth of active faces into — each in its own gutter-inset tile, in one
/// begin/flush — wrapped once as a single rive [`RenderTarget`]. The render world keys these
/// by [`AtlasPageId`] in `RiveGpu::atlas_pages`; the grid layout is the shared bucket helpers.
struct RiveAtlas {
    /// rive's render target wrapping `shared_tex`'s `VkImage` (zero copy).
    target: RenderTarget,
    /// The wgpu-owned shared atlas texture rive renders into. Held so its `VkImage`
    /// stays alive while rive references it.
    #[expect(
        dead_code,
        reason = "ownership anchor: keeps the VkImage alive for rive"
    )]
    shared_tex: Texture,
    /// A sampled view of `shared_tex` — the un-premult pass's source binding.
    shared_view: TextureView,
    /// Cached un-premult-pass bind group (source = the stable `shared_view`).
    bind_group: Option<wgpu::BindGroup>,
}

/// Per-entity rive state in the atlas path: the artboard + its state machine only.
/// The render target is the shared [`RiveAtlas`], so there is no per-instance texture.
struct AtlasInstance {
    artboard: Artboard,
    state_machine: StateMachine,
    /// Pointer edge-tracking (mirrors the dedicated [`RiveInstance`]'s fields):
    /// emit `pointer_down`/`pointer_up` on button edges and one `pointer_exit` when
    /// the cursor leaves. The inversion is tile-aware (see the atlas advance loop).
    last_pointer_down: bool,
    last_pointer_present: bool,
}

/// Identifies one atlas PAGE: the consumer's [`RiveAtlasKey`] `key` (distinct keys never
/// share a page — the documented pool isolation), the LOD `bucket` (index into
/// [`ATLAS_BUCKETS`], picked by face size WITHIN a key), and which sibling `page`. BOTH
/// worlds key pages by this — main-world holds the display image, render-world the
/// [`RiveAtlas`] — so they agree with no render→main channel.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct AtlasPageId {
    key: u32,
    bucket: u8,
    page: u16,
}

/// A face's atlas assignment: its [`AtlasPageId`] + tile `slot` within that page's grid.
#[derive(Clone, Copy)]
struct AtlasLoc {
    page: AtlasPageId,
    slot: u16,
}

/// Page dimension (px/side) for a bucket: `tile × ATLAS_TILES_PER_SIDE` (≤ 8192).
fn atlas_page_px(bucket: u8) -> u32 {
    ATLAS_BUCKETS[bucket as usize] * ATLAS_TILES_PER_SIDE
}

/// The gutter-inset pixel rect `[x, y, w, h]` of `slot` in its page — the DRAWN viewport
/// (`rive_artboard_draw_viewport` aligns + clips the artboard into exactly this). Inset by
/// [`ATLAS_GUTTER_PX`] per side so neighbouring tiles can't bleed across the clipRect AA edge.
fn atlas_tile_rect_px(bucket: u8, slot: u16) -> [f32; 4] {
    let tile = ATLAS_BUCKETS[bucket as usize];
    let (col, row) = (
        u32::from(slot) % ATLAS_TILES_PER_SIDE,
        u32::from(slot) / ATLAS_TILES_PER_SIDE,
    );
    let g = ATLAS_GUTTER_PX;
    [
        (col * tile + g) as f32,
        (row * tile + g) as f32,
        (tile - 2 * g) as f32,
        (tile - 2 * g) as f32,
    ]
}

/// Normalized `[0, 1]` uv-rect of `slot` in its page — the inset content region the consumer
/// samples. Matches [`atlas_tile_rect_px`] / [`atlas_page_px`], so the sampled sub-rect is
/// exactly what was drawn (no bleed, no half-tile offset).
fn atlas_tile_uv_rect(bucket: u8, slot: u16) -> Rect {
    let r = atlas_tile_rect_px(bucket, slot);
    let inv = 1.0 / atlas_page_px(bucket) as f32;
    Rect {
        min: Vec2::new(r[0] * inv, r[1] * inv),
        max: Vec2::new((r[0] + r[2]) * inv, (r[1] + r[3]) * inv),
    }
}

/// Smallest bucket index whose tile ≥ `max(width, height)`; clamps to the last (largest)
/// bucket — a face larger than the biggest tile renders contained into it (LOD downscale).
fn atlas_bucket_for(width: u32, height: u32) -> u8 {
    let want = width.max(height);
    ATLAS_BUCKETS
        .iter()
        .position(|&t| t >= want)
        .unwrap_or(ATLAS_BUCKETS.len() - 1) as u8
}

/// One sibling page within a bucket: its main-world display image + an O(1) slot allocator.
struct AtlasPageState {
    /// The shared straight-alpha display page (`Rgba8UnormSrgb`, `atlas_page_px(bucket)`²),
    /// written into every face-of-this-page's `RiveSurface.image`.
    display: Handle<Image>,
    /// Bump cursor (`0..ATLAS_TILES_PER_PAGE`) + freed-slot list (cull / despawn reclaim).
    next: u32,
    free: Vec<u16>,
}

/// One LOD bucket: a list of sibling pages (all the same tile size). Grown by appending a
/// page when every existing page is full — never by enlarging a page (the C1 VRAM budget).
#[derive(Default)]
struct AtlasBucketState {
    pages: Vec<AtlasPageState>,
}

/// MAIN-world atlas bookkeeping (M-SCALE Phase 2b → 3 multi-page → 4 keyed): per-`(key,
/// bucket)` sibling pages plus per-entity slot assignment. Lives main-world so a face's
/// `image` and `uv_rect` (its [`RiveSurface`]) are written together, the SAME frame, BEFORE
/// any consumer reads — the render world only READS the assigned [`AtlasLoc`] (no render→main
/// path). Pools are keyed by `(RiveAtlasKey value, size-bucket)`, so distinct keys never
/// share a page (the documented isolation) and size picks the LOD bucket within a key.
#[derive(Resource, Default)]
struct RiveAtlasState {
    /// Sibling pages per `(RiveAtlasKey value, size-bucket)`, created lazily.
    pools: HashMap<(u32, u8), AtlasBucketState>,
    /// Assigned location per atlas-opted entity (read by `extract_rive_instances`).
    locs: HashMap<Entity, AtlasLoc>,
}

impl RiveAtlasState {
    /// Allocates a free tile in pool `(key, bucket)` (reusing a freed slot, else bumping a
    /// page's cursor, else appending a sibling page whose display image is created now).
    /// Returns the assigned location. `images` is needed only to create a new page's image.
    fn alloc(&mut self, key: u32, bucket: u8, images: &mut Assets<Image>) -> AtlasLoc {
        let pages = &mut self.pools.entry((key, bucket)).or_default().pages;
        for (pi, page) in pages.iter_mut().enumerate() {
            if let Some(slot) = page.free.pop() {
                return AtlasLoc {
                    page: AtlasPageId {
                        key,
                        bucket,
                        page: pi as u16,
                    },
                    slot,
                };
            }
            if page.next < ATLAS_TILES_PER_PAGE {
                let slot = page.next as u16;
                page.next += 1;
                return AtlasLoc {
                    page: AtlasPageId {
                        key,
                        bucket,
                        page: pi as u16,
                    },
                    slot,
                };
            }
        }
        // Every page full → append a sibling page (slot 0).
        let px = atlas_page_px(bucket);
        pages.push(AtlasPageState {
            display: images.add(make_display_image(px, px)),
            next: 1,
            free: Vec::new(),
        });
        AtlasLoc {
            page: AtlasPageId {
                key,
                bucket,
                page: (pages.len() - 1) as u16,
            },
            slot: 0,
        }
    }

    /// Returns a slot to its page's free-list (cull / despawn). The page itself persists
    /// (occupancy is bounded by the active set, so we don't churn page textures).
    fn free(&mut self, loc: AtlasLoc) {
        if let Some(bucket) = self.pools.get_mut(&(loc.page.key, loc.page.bucket)) {
            bucket.pages[loc.page.page as usize].free.push(loc.slot);
        }
    }

    /// The display image handle for a location's page.
    fn display_of(&self, page: AtlasPageId) -> Handle<Image> {
        self.pools[&(page.key, page.bucket)].pages[page.page as usize]
            .display
            .clone()
    }
}

/// The fullscreen un-premultiply + sRGB-decode blit pipeline (M1b display).
///
/// rive renders **premultiplied, sRGB-encoded** bytes into the shared
/// `Rgba8Unorm` texture. This pass samples those raw bytes (linear format → no
/// hardware decode), un-premultiplies in encoded space, sRGB-decodes, and writes
/// the **linear straight** result to the `Rgba8UnormSrgb` display texture (whose
/// store re-applies the sRGB OETF). The `Sprite` then hardware-decodes on sample
/// → linear straight → composites with the correct straight-alpha OVER, matching
/// M1a pixel-for-pixel including partial alpha (design spec §7 Option B).
///
/// Raw wgpu (not `PipelineCache`): self-contained, no asset-load timing, and the
/// node already drives raw wgpu. wgpu objects are `Send + Sync` on native.
struct RiveBlitPipeline {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
}

/// WGSL for the un-premultiply + sRGB-decode fullscreen pass. A 3-vertex
/// fullscreen triangle (no vertex buffer) + the per-channel straight-alpha math.
const UNPREMULT_WGSL: &str = r#"
@vertex
fn vs(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    // Fullscreen triangle (no vertex buffer): clip in [-1,3]/[3,-1].
    let x = f32((vi << 1u) & 2u);
    let y = f32(vi & 2u);
    return vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
}

@group(0) @binding(0) var src: texture_2d<f32>;

fn srgb_decode(x: f32) -> f32 {
    if (x <= 0.04045) { return x / 12.92; }
    return pow((x + 0.055) / 1.055, 2.4);
}

@fragment
fn fs(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    // 1:1 same-size blit: index the source by integer destination pixel coords
    // (no sampler, matching the texture-only bind-group layout). src is
    // Rgba8Unorm (linear format) so we read rive's raw encoded, premultiplied
    // bytes verbatim — no hardware sRGB decode here.
    let c = textureLoad(src, vec2<i32>(pos.xy), 0);
    let a = c.a;
    // Un-premultiply in ENCODED space (matches M1a's integer round(c*255/a)).
    let straight = select(c.rgb / a, vec3<f32>(0.0), a == 0.0);
    // Decode to linear; the Rgba8UnormSrgb target re-encodes on store, so the
    // stored bytes == straight sRGB-encoded == what the M1a Sprite path expects.
    let lin = vec3<f32>(srgb_decode(straight.r), srgb_decode(straight.g), srgb_decode(straight.b));
    return vec4<f32>(lin, a);
}
"#;

impl RiveBlitPipeline {
    /// Build the pipeline against the live wgpu device. Called lazily from the
    /// render node (not `FromWorld`, which would run too early — see `RiveGpu`).
    fn new(device: &wgpu::Device) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rive_unpremult_shader"),
            source: wgpu::ShaderSource::Wgsl(UNPREMULT_WGSL.into()),
        });

        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("rive_unpremult_bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    // textureLoad doesn't filter, but Float{filterable:true} matches
                    // the Rgba8Unorm view; the sample_type is what wgpu validates.
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            }],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rive_unpremult_pl"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("rive_unpremult_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                // REPLACE (blend: None): the pass fully overwrites the display texture.
                targets: &[Some(wgpu::ColorTargetState {
                    format: DISPLAY_FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview: None,
            cache: None,
        });

        Self { pipeline, layout }
    }
}

/// M2a per-frame perf collector for the rive fill node.
///
/// Collects, **per frame** (summed across every rive instance rendered that
/// frame): the CPU wall of the rive submit calls
/// ([`Context::render_external_frame`]), split via the shim into rive's CPU
/// `flush()` and the **blocking fence wait** (the stall the M2a sync rework
/// removes), plus rive's GPU command-buffer time (Vulkan timestamps, when
/// available). After a warm-up it gathers `target` frames then logs a
/// median+percentile summary once. Enabled by `RIVE_PERF`.
///
/// Recording per *frame* (not per submit) lets one mechanism serve both Step 0
/// measurements: at N=1 the frame totals are that instance's fence-vs-flush split;
/// at N>1 the frame CPU total is the frame-time-vs-N scaling point.
#[derive(Default)]
struct PerfStats {
    enabled: bool,
    target: u32,
    /// Warm-up frames to skip before collecting (first frames include lazy
    /// pipeline/context/instance creation, so they are not representative).
    warmup: u32,
    seen: u32,
    /// Instance count of the most recent recorded frame (the run's N).
    instances: u32,
    /// M-SCALE: per-frame state-machine `advance` total (CPU tick), summed over
    /// instances. Runs *before* the record span so it is invisible to
    /// `frame_cpu_us` — the term that decides whether threading `advance` pays off.
    frame_advance_us: Vec<f64>,
    /// Per-frame rive submit wall, summed over the frame's instances.
    frame_cpu_us: Vec<f64>,
    /// Per-frame rive CPU `flush()` total (shim-measured), summed over instances.
    frame_flush_us: Vec<f64>,
    /// Per-frame blocking fence-wait total (shim-measured), summed over instances.
    frame_fence_us: Vec<f64>,
    /// Per-frame rive GPU command-buffer total, summed over instances.
    frame_gpu_ms: Vec<f64>,
    /// M-SCALE: per-frame un-premult blit-encode total (cached-bind-group lookup +
    /// the fullscreen pass), summed over instances. Runs *after* the record span.
    frame_blit_us: Vec<f64>,
    /// M2b diagnostic — per-frame CPU run-ahead = `current_frame - safe_frame` (frames
    /// submitted but not yet GPU-complete). Correlates flush growth/variance with how
    /// far the CPU outruns GPU completion. Real with the exact timeline watermark;
    /// ≈ constant `RIVE_RING_SIZE` with the fixed-ring fallback.
    frame_run_ahead: Vec<f64>,
    /// M2c Step 2 — per-frame wall-clock period [us] between consecutive *measured*
    /// (post-warm-up) frames. Under a run-ahead-permitting present mode (Immediate,
    /// no vsync) this is the sustained-throughput indicator (fps = 1e6 / period);
    /// under Fifo it is pinned at the display refresh interval and not meaningful.
    frame_period_us: Vec<f64>,
    /// Wall-clock instant of the previous measured frame, to diff into
    /// `frame_period_us`. `None` until the first post-warm-up frame seeds it.
    last_frame_at: Option<std::time::Instant>,
    summarized: bool,
}

/// One frame's aggregate per-phase timings, each summed over the frame's instances.
/// Named fields (not positional `f64`s) so the per-phase split — advance | record
/// (`cpu`/`flush`) | `blit` — can't be transposed at the call site.
struct FrameTimings {
    instances: u32,
    advance_us: f64,
    cpu_us: f64,
    flush_us: f64,
    fence_us: f64,
    gpu_ms: Option<f64>,
    blit_us: f64,
    run_ahead: f64,
}

impl PerfStats {
    /// Record one frame's aggregate timings (summed over the frame's instances).
    /// `gpu_ms` is `None` if GPU timing was unavailable for any submit this frame.
    fn record_frame(&mut self, t: FrameTimings) {
        if !self.enabled || self.summarized {
            return;
        }
        self.seen += 1;
        if self.seen <= self.warmup {
            return;
        }
        // M2c Step 2: sample the steady-state wall-clock frame period → sustained
        // throughput. The first measured frame only seeds the reference instant.
        let now = std::time::Instant::now();
        if let Some(prev) = self.last_frame_at {
            self.frame_period_us
                .push(now.duration_since(prev).as_secs_f64() * 1e6);
        }
        self.last_frame_at = Some(now);
        self.instances = t.instances;
        self.frame_advance_us.push(t.advance_us);
        self.frame_cpu_us.push(t.cpu_us);
        self.frame_flush_us.push(t.flush_us);
        self.frame_fence_us.push(t.fence_us);
        self.frame_blit_us.push(t.blit_us);
        self.frame_run_ahead.push(t.run_ahead);
        if let Some(ms) = t.gpu_ms {
            self.frame_gpu_ms.push(ms);
        }
        if self.frame_cpu_us.len() as u32 >= self.target {
            self.summarize();
            self.summarized = true;
        }
    }

    fn summarize(&self) {
        let cpu = Summary::of(&self.frame_cpu_us);
        let advance = Summary::of(&self.frame_advance_us);
        let blit = Summary::of(&self.frame_blit_us);
        let flush = Summary::of(&self.frame_flush_us);
        let fence = Summary::of(&self.frame_fence_us);
        let gpu = Summary::of(&self.frame_gpu_ms);
        let run_ahead = Summary::of(&self.frame_run_ahead);
        let period = Summary::of(&self.frame_period_us);
        // Sustained throughput from the steady-state period (Immediate/no-vsync only;
        // under Fifo this is pinned at the refresh rate).
        let fps_p50 = if period.p50 > 0.0 {
            1e6 / period.p50
        } else {
            0.0
        };
        info!(
            "rive zero-copy PERF (frames={}, instances={}): advance [us] {} | \
             frame CPU/record [us] {} | rive flush [us] {} | fence wait [us] {} | \
             rive GPU [ms] {} | blit [us] {} | run-ahead [frames] {} | \
             frame period [us] {} | fps(p50)={:.1}",
            cpu.n,
            self.instances,
            advance.fmt_us(),
            cpu.fmt_us(),
            flush.fmt_us(),
            fence.fmt_us(),
            if gpu.n > 0 {
                gpu.fmt_ms()
            } else {
                "unavailable".to_string()
            },
            blit.fmt_us(),
            run_ahead.fmt_us(),
            period.fmt_us(),
            fps_p50,
        );
    }
}

/// median/percentile summary of a sample set (computed on a sorted copy).
struct Summary {
    n: usize,
    p50: f64,
    p90: f64,
    p95: f64,
    p99: f64,
    min: f64,
    max: f64,
    mean: f64,
}

impl Summary {
    fn of(samples: &[f64]) -> Self {
        if samples.is_empty() {
            return Self {
                n: 0,
                p50: 0.0,
                p90: 0.0,
                p95: 0.0,
                p99: 0.0,
                min: 0.0,
                max: 0.0,
                mean: 0.0,
            };
        }
        let mut s = samples.to_vec();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        // Nearest-rank percentile: ceil(p*n)-1, clamped.
        let pct = |p: f64| {
            let idx = ((p * s.len() as f64).ceil() as usize).saturating_sub(1);
            s[idx.min(s.len() - 1)]
        };
        let mean = s.iter().sum::<f64>() / s.len() as f64;
        Self {
            n: s.len(),
            p50: pct(0.50),
            p90: pct(0.90),
            p95: pct(0.95),
            p99: pct(0.99),
            min: s[0],
            max: s[s.len() - 1],
            mean,
        }
    }
    fn fmt_us(&self) -> String {
        format!(
            "p50={:.1} p90={:.1} p95={:.1} p99={:.1} min={:.1} max={:.1} mean={:.1}",
            self.p50, self.p90, self.p95, self.p99, self.min, self.max, self.mean
        )
    }
    fn fmt_ms(&self) -> String {
        format!(
            "p50={:.3} p90={:.3} p95={:.3} p99={:.3} min={:.3} max={:.3} mean={:.3}",
            self.p50, self.p90, self.p95, self.p99, self.min, self.max, self.mean
        )
    }
}

/// The render-world owner of rive's `!Send` objects.
///
/// Held as a `NonSend` render-world resource (see [`RiveRenderState`]). This tier
/// disables pipelined rendering, so the render schedule — and the *drop* of this
/// resource — run on the **main thread**; the rive handles' non-atomic `Rc`
/// refcount is therefore sound and no `unsafe Send` is needed. Touched only inside
/// [`extract_shared_handles_once`] / [`RiveFillNode::run`].
struct RiveGpu {
    /// The external rive context on wgpu's device. Lazily created in the node.
    ctx: Option<Context>,
    /// The un-premult display pipeline. Lazily built in the node: its constructor
    /// needs `RenderDevice`, which only exists after `RenderPlugin::finish()` — so
    /// it cannot be an `init_resource`/`FromWorld` created during plugin `build()`.
    blit: Option<RiveBlitPipeline>,
    instances: HashMap<Entity, RiveInstance>,
    /// Per-entity rive state for the atlas path (artboard + state machine only; the
    /// render target is the shared `atlas`, not a per-instance texture).
    atlas_instances: HashMap<Entity, AtlasInstance>,
    /// The shared atlas pages (render-world), keyed by [`AtlasPageId`]: every active atlas
    /// face renders into its page's ONE texture in one begin/flush. Lazily built per page in
    /// the node, sized from the extracted `page_px` (per-LOD-bucket). O(pages) flushes/blits.
    ///
    /// TEARDOWN: the load-bearing invariant is PER-PAGE and lives inside [`RiveAtlas`] —
    /// `target` is declared before `shared_tex`, so each rive `RenderTarget` is destroyed
    /// before the wgpu texture whose `VkImage` it wraps (else `rive_render_target_destroy`
    /// touches a freed image). The rive *context* destroy is independent of field order: it
    /// is deferred by the `Rc<ContextInner>` refcount (it runs only when the LAST handle —
    /// any `RenderTarget`/`Artboard`/`StateMachine` across these maps — drops), so neither
    /// this field's position nor the unspecified `HashMap` value-drop order affects it. The
    /// original single-`atlas` ordering rationale (drop the texture last) is therefore moot
    /// for multi-page; the per-page `target`-before-`shared_tex` rule is what matters.
    atlas_pages: HashMap<AtlasPageId, RiveAtlas>,
    /// Monotonic frame counter for rive's resource-recycling watermark.
    frame: u64,
    /// M-DATA: the [`ExtractedRives::generation`] this node last applied VM writes for.
    /// The node can run more than once per visual frame (one run per camera sub-graph),
    /// so writes — notably the non-idempotent `fire_trigger` — are applied only when this
    /// differs from the current generation, then it is advanced. 0 never collides (extract
    /// starts the generation at 1).
    vm_writes_applied_gen: u64,
    /// Set once we have logged the active PLS mode.
    logged_mode: bool,
    /// M2b: tracks whether the fixed-ring fallback's recycle precondition is currently
    /// violated (a non-vsync present mode / high frame-latency). Lets the warning re-fire
    /// when the live window config transitions INTO the unsafe regime at runtime (e.g. the
    /// user toggles vsync off), instead of latching on the first frame. Unused while the
    /// exact timeline-semaphore watermark is active (no present-mode precondition then).
    recycle_unsafe_warned: bool,
    /// Set once we have applied the clockwise override to the context.
    clockwise_applied: bool,
    /// M2.0 perf collector (configured from `RiveSharedHandles` on first frame).
    perf: PerfStats,
}

/// `NonSend` render-world resource wrapping the `!Send` [`RiveGpu`] behind a
/// `RefCell` for interior mutability (the node gets `&World`, so it can only take
/// `&RiveRenderState` and must `borrow_mut`). Being `NonSend` — not a `Resource`
/// (which would require `Send + Sync`) — is exactly what lets it hold the
/// `!Send + !Sync` rive handles **without** an `unsafe Send` assertion. Sound
/// because this tier runs the render world on the main thread (pipelined rendering
/// disabled), so the resource is only ever accessed and dropped on one thread.
struct RiveRenderState(RefCell<RiveGpu>);

impl Default for RiveRenderState {
    fn default() -> Self {
        Self(RefCell::new(RiveGpu {
            ctx: None,
            blit: None,
            instances: HashMap::new(),
            atlas_instances: HashMap::new(),
            atlas_pages: HashMap::new(),
            frame: 0,
            vm_writes_applied_gen: 0,
            logged_mode: false,
            recycle_unsafe_warned: false,
            clockwise_applied: false,
            perf: PerfStats::default(),
        }))
    }
}

/// Per-frame atlas placement for one face, resolved MAIN-world and ferried to the render
/// node. Carries everything the node needs WITHOUT any render→main channel or knowledge of
/// the bucket config: which `page` to group/key by, the gutter-inset `tile_rect` to draw
/// into, the `page_px` to size that page's texture, and the `display` page to blit into.
#[derive(Clone)]
struct ExtractedAtlas {
    page: AtlasPageId,
    tile_rect: [f32; 4],
    page_px: u32,
    display: Handle<Image>,
}

/// Per-entity `Send` data ferried main→render each frame by [`extract_rive_instances`].
///
/// Carries the **main-world** [`Entity`] as a stable per-instance key. We collect
/// these into the [`ExtractedRives`] render-world resource rather than inserting
/// per-entity render-world components, because the rive entities are not
/// render-world-synced (we don't use `ExtractComponentPlugin`), so there is no
/// `RenderEntity` to key components onto.
#[derive(Clone)]
struct ExtractedRive {
    entity: Entity,
    display: Handle<Image>,
    bytes: std::sync::Arc<[u8]>,
    width: u32,
    height: u32,
    /// `dt * speed`, sanitized non-negative + finite (0 for a culled face).
    step: f32,
    /// M-SCALE: `Some` for an atlas-opted face (its resolved tile placement, assigned
    /// main-world); `None` for a dedicated-image face (which uses `display` above) OR a
    /// culled face (no placement this frame).
    atlas: Option<ExtractedAtlas>,
    /// M-SCALE: `RiveActive(false)` — the face is PAUSED this frame (skip advance + record),
    /// but still ferried so the node keeps it in the live set and does NOT evict its rive
    /// state machine. Without this, `retain` would drop a culled-but-alive face and the next
    /// activation would rebuild it (a t=0 reset + `.riv` re-parse). Its atlas tile is freed
    /// main-world regardless; only the rive state is preserved for resume-in-place.
    culled: bool,
    /// M-DATA: this frame's view-model writes (from a `RiveViewModel`), applied to
    /// the instance in the node before advance — the zero-copy analogue of the
    /// floor advance system's inline `apply_writes`. Empty for faces with no
    /// `RiveViewModel` or no queued writes. Reads/watch are not ferried (floor-only).
    vm_writes: Vec<(String, RiveValue)>,
    /// M-TEXT: this frame's text-run set writes (from a `RiveText`), applied to
    /// the instance in the node before advance — the zero-copy analogue of the
    /// floor advance system's inline `apply_text_writes`. Empty for faces with no
    /// `RiveText` or no queued writes. Staged main-world by `stage_text_writes`.
    text_writes: Vec<crate::text::TextWrite>,
    /// Artboard / state-machine selectors (default / by name / by index), honored
    /// once when the node first builds this entity's instance. Ferried each frame
    /// but read only at build time; cheap for the common `Default` (a bare enum —
    /// a `String` clone only when `ByName` is used).
    artboard_sel: crate::ArtboardSelector,
    state_machine_sel: crate::StateMachineSelector,
    /// Out-of-band assets (the `RiveAssets` component) supplied to the `.riv` by
    /// authored name. Like the selectors, honored once when the node first builds
    /// this entity's instance; ferried each frame as a cheap `Arc` refcount bump
    /// (`None` when the entity has no `RiveAssets`).
    assets: Option<crate::RiveAssets>,
    /// How this face is scaled/aligned into its target (the `RiveFit` component;
    /// `Default` = contain/center). Applied to the instance's artboard each frame
    /// before draw — on BOTH the dedicated path (full target) and atlas tiles
    /// (within the tile rect, via draw_viewport). Copy + cheap.
    fit_align: FitAlign,
    /// This frame's pointer input (from a `RivePointer`), in TARGET-PIXEL space.
    /// `None` = off-surface or no `RivePointer` ⇒ `pointer_exit`. Forwarded to the
    /// state machine's Listeners before advance on BOTH zero_copy draw paths: the
    /// dedicated path inverts against the full target; the atlas path draws into a
    /// tile sub-rect (`draw_viewport`) and inverts tile-aware (the node sets the
    /// tile size via `set_pointer_tile`; see the atlas advance loop). Copy + cheap.
    pointer: Option<Vec2>,
    /// Whether the primary button is held this frame (paired with `pointer`).
    pointer_down: bool,
}

/// Render-world resource holding this frame's extracted rive instances. Replaced
/// wholesale each frame by [`extract_rive_instances`]; read by [`RiveFillNode`].
#[derive(Resource, Default)]
struct ExtractedRives {
    items: Vec<ExtractedRive>,
    /// Bumped once per frame by [`extract_rive_instances`]. The node may run more than
    /// once per frame (one run per camera sub-graph under `RiveGraphAnchor::Both`, or
    /// multiple cameras on one anchored graph); it uses this generation to apply
    /// non-idempotent VM writes (notably `fire_trigger`) exactly once per visual frame.
    generation: u64,
}

// ===========================================================================
// Main-world system: allocate the display Image (the frozen seam).
// ===========================================================================

/// Allocates the display [`Image`] for each M1b entity whose `.riv` has loaded
/// and whose [`RiveTarget`] has no image yet, then writes the handle back. The
/// image is GPU-only (`data: None`), `Rgba8UnormSrgb`, with the usages the
/// un-premultiply display pass + the sprite sample need.
fn allocate_display_images(
    mut query: Query<(&RiveAnimation, &mut RiveTarget)>,
    files: Res<Assets<RiveFile>>,
    mut images: ResMut<Assets<Image>>,
) {
    for (anim, mut target) in &mut query {
        if target.image != Handle::default() {
            continue;
        }
        if target.atlas.is_some() {
            continue; // atlas faces sample the shared atlas (RiveSurface), not a per-face image
        }
        if files.get(&anim.handle).is_none() {
            continue; // not loaded yet
        }
        target.image = images.add(make_display_image(target.width, target.height));
    }
}

/// Assigns a per-LOD-bucket atlas slot + writes [`RiveSurface`] for each ACTIVE opt-in
/// atlas face (`RiveTarget::atlas == Some`, [`RiveActive`] absent-or-true), frees + drops
/// `RiveSurface` for culled (inactive) faces, and reclaims the slots of despawned faces.
/// MAIN-world (`Update`): `image` + `uv_rect` are written together so a single-shot
/// consumer never latches a stale UV; the bucket is `max(width, height)` → smallest tile.
fn allocate_atlas_slots(
    mut commands: Commands,
    mut state: ResMut<RiveAtlasState>,
    mut images: ResMut<Assets<Image>>,
    query: Query<(Entity, &RiveTarget, Option<&RiveActive>), With<RiveAnimation>>,
) {
    let mut seen: std::collections::HashSet<Entity> = std::collections::HashSet::new();
    for (entity, target, active) in &query {
        let Some(key) = target.atlas else {
            continue; // dedicated face (RiveTarget.atlas == None)
        };
        seen.insert(entity);
        let active = active.is_none_or(|a| a.0);
        let has_loc = state.locs.contains_key(&entity);
        if active && !has_loc {
            // Allocate a tile in this key's size-appropriate bucket (creates a sibling page
            // + its display image lazily) and publish the seam — image + uv_rect together.
            let bucket = atlas_bucket_for(target.width, target.height);
            let loc = state.alloc(key.0, bucket, &mut images);
            state.locs.insert(entity, loc);
            commands.entity(entity).insert(RiveSurface {
                image: state.display_of(loc.page),
                uv_rect: atlas_tile_uv_rect(bucket, loc.slot),
                atlas_size: UVec2::splat(atlas_page_px(bucket)),
            });
        } else if !active && has_loc {
            // Culled: free the tile (so the page slot can be reused) and drop RiveSurface
            // so the consumer stops sampling a tile that another face may now own.
            if let Some(loc) = state.locs.remove(&entity) {
                state.free(loc);
            }
            commands.entity(entity).remove::<RiveSurface>();
        }
    }
    // Reclaim slots of atlas faces that have despawned (gone from the world this frame).
    let stale: Vec<(Entity, AtlasLoc)> = state
        .locs
        .iter()
        .filter(|(e, _)| !seen.contains(*e))
        .map(|(e, l)| (*e, *l))
        .collect();
    for (entity, loc) in stale {
        state.locs.remove(&entity);
        state.free(loc);
    }
}

/// A GPU-only `Rgba8UnormSrgb` display image (`data: None`), with usages for the
/// un-premult pass target (`RENDER_ATTACHMENT`), sprite sampling
/// (`TEXTURE_BINDING`), and capture readback (`COPY_SRC`).
fn make_display_image(width: u32, height: u32) -> Image {
    let mut image = Image::new_uninit(
        Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        DISPLAY_FORMAT,
        RenderAssetUsages::RENDER_WORLD,
    );
    image.texture_descriptor.usage =
        TextureUsages::RENDER_ATTACHMENT | TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_SRC;
    image
}

// ===========================================================================
// Render-world: extract handles + per-entity data.
// ===========================================================================

/// Extracts the shared Vulkan handles from Bevy's wgpu device exactly once and
/// inserts [`RiveSharedHandles`]. Runs in `RenderSystems::Prepare` and no-ops
/// after the first success.
#[expect(
    clippy::needless_pass_by_value,
    reason = "Bevy systems take resources by value (Res/Option<Res>)"
)]
fn extract_shared_handles_once(
    mut commands: Commands,
    existing: Option<Res<RiveSharedHandles>>,
    device: Res<RenderDevice>,
    adapter: Res<RenderAdapter>,
    instance: Res<RenderInstance>,
    additional: Option<Res<AdditionalVulkanFeatures>>,
    // Log-once latch (M-PKG.1 backend guard): the wrong-backend diagnostic below must
    // fire ONCE, not every frame this no-op system re-runs waiting for the handles.
    mut backend_warned: Local<bool>,
) {
    if existing.is_some() {
        return;
    }
    // SAFETY: the guards are held only for the extraction; raw handles are copied
    // out as integers and remain valid while Bevy's RenderDevice lives (which
    // outlives the render world). We never store the guards.
    let handles = unsafe {
        let Some(dev_g) = device.wgpu_device().as_hal::<Vk>() else {
            // The zero-copy handoff needs `as_hal::<Vulkan>`; on any other backend it
            // returns None and the tier is INERT. Make that loud + actionable, once —
            // this is the runtime half of the M-PKG.1 fail-fast guard (the compile-time
            // half is the exact wgpu pins; see build.rs).
            if !*backend_warned {
                *backend_warned = true;
                error!(
                    "rive zero-copy: wgpu is NOT on the Vulkan backend — the shared-VkImage \
                     fast path is INERT (nothing renders via RiveZeroCopyPlugin). Set \
                     `WGPU_BACKEND=vulkan` (D3D12/Metal are not yet supported; see \
                     docs/M3_0_D3D12_SPIKE.md), or use the default `floor` tier instead."
                );
            }
            return;
        };
        let vk_device = dev_g.raw_device().handle();
        let vk_queue = dev_g.raw_queue();
        let qfi = dev_g.queue_family_index();
        let enabled_exts: Vec<&CStr> = dev_g.enabled_device_extensions().to_vec();
        let inst_shared = dev_g.shared_instance();
        let vk_instance = inst_shared.raw_instance().handle();
        let gipa = inst_shared.entry().static_fn().get_instance_proc_addr;

        let Some(adapter_g) = adapter.as_hal::<Vk>() else {
            if !*backend_warned {
                *backend_warned = true;
                error!(
                    "rive zero-copy: wgpu adapter is not Vulkan — fast path inert \
                     (set `WGPU_BACKEND=vulkan`)."
                );
            }
            return;
        };
        let vk_phys = adapter_g.raw_physical_device();
        let _ = &instance; // RenderInstance kept as a dep to assert Vulkan init order

        let ext_enabled = |name: &CStr| enabled_exts.contains(&name);
        // `RIVE_FORCE_ATOMIC` forces the atomic PLS path AND suppresses the
        // interlock feature flags, so rive never records interlock commands a
        // non-conformant device (Dozen) can't execute. Native testing leaves it
        // unset to exercise the real raster-order path.
        let force_atomic = std::env::var_os("RIVE_FORCE_ATOMIC").is_some();
        let pixel = !force_atomic && ext_enabled(ash::ext::fragment_shader_interlock::NAME);
        let raster =
            !force_atomic && ext_enabled(ash::ext::rasterization_order_attachment_access::NAME);

        let mut features = VulkanFeatures {
            // rive requires these for core operation; wgpu enables them.
            fragment_stores_and_atomics: true,
            fill_mode_non_solid: true,
            independent_blend: true,
            fragment_shader_pixel_interlock: pixel,
            rasterization_order_color_attachment_access: raster,
            ..VulkanFeatures::default()
        };
        // VK_API_VERSION_1_1 default in VulkanFeatures is fine; rive only needs >= 1.1.
        features.api_version = 0x0040_1000;

        // M2.0 perf knobs (read once at handle extraction).
        // M2c: capability-gated clockwise default. rive's clockwise PLS path needs
        // pixel interlock; where it's available — AND raster-order is not, since rive
        // prefers the cleaner RasterOrdering when that ext is present — clockwise is
        // strictly-better-or-neutral: lower rive CPU + flush at every N (Step 1) and
        // +8–13% sustained throughput in the GPU-leaning N≈32–128 band (Step 2), with
        // ≤1 LSB / alpha-identical output vs atomics (Step 3, octopus + coffee). So
        // default it wherever `pixel && !raster`. Overrides (kept for A/B + forcing
        // either path):
        //   RIVE_NO_CLOCKWISE → atomics on an interlock device (the Step-1/2 A/B baseline)
        //   RIVE_CLOCKWISE    → force clockwise on
        //   RIVE_FORCE_ATOMIC → clears `pixel`/`raster` above (and suppresses the exts)
        // Non-interlock devices fall back to atomics automatically with no flag: Dozen
        // advertises neither ext (pixel=raster=false — verified VUID/hazard-clean), as
        // does older HW.
        let clockwise = if std::env::var_os("RIVE_NO_CLOCKWISE").is_some() {
            false
        } else if std::env::var_os("RIVE_CLOCKWISE").is_some() {
            true
        } else {
            pixel && !raster
        };
        let perf_enabled = std::env::var_os("RIVE_PERF").is_some();
        let perf_target = std::env::var("RIVE_PERF_FRAMES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(300u32)
            .max(1);
        // M2a: default to the non-blocking record-into-wgpu path; RIVE_BLOCKING=1
        // selects the M1b blocking submit+fence (fallback / A-B baseline).
        let blocking_submit = std::env::var_os("RIVE_BLOCKING").is_some();

        // M2b: for the non-blocking path, create our own Vulkan TIMELINE semaphore to
        // get an EXACT, non-blocking GPU-completion watermark. wgpu signals it with the
        // frame number on each per-frame submit (see the node), so reading its counter
        // tells rive precisely which frames' transient buffers are free to recycle,
        // removing the M2a "≤ ring frames in flight" precondition. Falls back to `0`
        // (fixed `frame - RIVE_RING_SIZE`) for the blocking path, a device lacking
        // timeline semaphores (Vulkan 1.2 core / VK_KHR_timeline_semaphore), or the
        // `RIVE_NO_WATERMARK` A/B knob (forces the fixed-ring path on one build, to
        // measure the watermark's effect on rive's flush — M2b Step 2).
        let frame_sync_sema = if blocking_submit || std::env::var_os("RIVE_NO_WATERMARK").is_some()
        {
            0
        } else {
            // Query support first — using a timeline semaphore without the feature is
            // UB. (features2 is Vulkan 1.1 core; wgpu's instance is ≥ 1.1 on desktop.)
            let mut ts_features = vk::PhysicalDeviceTimelineSemaphoreFeatures::default();
            {
                let mut features2 =
                    vk::PhysicalDeviceFeatures2::default().push_next(&mut ts_features);
                inst_shared
                    .raw_instance()
                    .get_physical_device_features2(vk_phys, &mut features2);
            }
            if ts_features.timeline_semaphore == vk::TRUE {
                let mut type_info = vk::SemaphoreTypeCreateInfo::default()
                    .semaphore_type(vk::SemaphoreType::TIMELINE)
                    .initial_value(0);
                let create_info = vk::SemaphoreCreateInfo::default().push_next(&mut type_info);
                match dev_g.raw_device().create_semaphore(&create_info, None) {
                    Ok(sema) => sema.as_raw(),
                    Err(e) => {
                        warn!(
                            "rive zero-copy: timeline semaphore create failed ({e:?}); \
                             falling back to the fixed frame-ring watermark"
                        );
                        0
                    }
                }
            } else {
                info!(
                    "rive zero-copy: device lacks timeline semaphores; using the fixed \
                     frame-ring watermark (vsync-bounded surface assumed — see RIVE_RING_SIZE)"
                );
                0
            }
        };

        RiveSharedHandles {
            instance: vk_instance.as_raw(),
            physical_device: vk_phys.as_raw(),
            device: vk_device.as_raw(),
            queue: vk_queue.as_raw(),
            queue_family_index: qfi,
            get_instance_proc_addr: gipa as usize,
            features,
            interlock: pixel || raster,
            force_atomic,
            clockwise,
            perf_enabled,
            perf_target,
            blocking_submit,
            frame_sync_sema,
        }
    };

    let from_marker = additional.is_some_and(|a| a.has::<RiveInterlock>());
    if handles.interlock || from_marker {
        // Report the features actually enabled (mirrors the device's enabled
        // extension set). rive's raster-ordering mode requires
        // `rasterization_order_color_attachment_access`; `fragment_shader_pixel_interlock`
        // only feeds its lower-priority clockwise mode — so the latter alone does
        // NOT yield raster-order (the earlier "expecting raster-order" log was wrong).
        info!(
            "rive zero-copy: interlock enabled — rasterization_order_color_attachment_access={} \
             (rive uses RasterOrdering iff this is true), fragment_shader_pixel_interlock={} \
             (clockwise mode only)",
            handles.features.rasterization_order_color_attachment_access,
            handles.features.fragment_shader_pixel_interlock,
        );
    } else {
        warn!("rive zero-copy: no interlock extension enabled — rive uses the atomic PLS fallback");
    }
    if handles.frame_sync_sema != 0 {
        // Give the timeline semaphore an owner that destroys it on teardown (its retained
        // device clone keeps the VkDevice alive across that — see RiveFrameSync).
        commands.insert_resource(RiveFrameSync {
            sema: handles.frame_sync_sema,
            device: device.wgpu_device().clone(),
        });
    }
    commands.insert_resource(handles);
}

/// M-DATA: double-buffers each `RiveViewModel`'s queued writes into its staging
/// slot, so the read-only [`extract_rive_instances`] can ferry them to the render
/// world (where this tier's instances live and advance). Runs in `Last` — after
/// gameplay queued writes this frame, before the render app's extract.
///
/// CRUCIAL: only drain `writes` → `staged` for a face that is **extractable this
/// frame** (asset loaded, surface allocated, not culled — the SAME readiness gate
/// [`extract_rive_instances`] applies below). Otherwise the writes stay queued in
/// `writes`, RETAINED across the async-load / unallocated-surface / culled window —
/// exactly as the floor advance system retains writes until the instance is live.
/// Draining unconditionally would strand a one-shot write (e.g. set initial state /
/// `fire_trigger` at spawn) in `staged`, which `stage_writes` then clears the next
/// frame before extract ever ferried it — a silent lost update.
fn stage_vm_writes(
    mut query: Query<(
        Entity,
        &RiveAnimation,
        &RiveTarget,
        Option<&RiveActive>,
        &mut RiveViewModel,
    )>,
    files: Res<Assets<RiveFile>>,
    atlas: Res<RiveAtlasState>,
) {
    for (entity, anim, target, active, mut vm) in &mut query {
        if !vm.has_staging_work() {
            continue;
        }
        // Mirror extract_rive_instances' readiness gate (asset loaded + not culled +
        // surface allocated). Keep these two in sync.
        let ready = files.get(&anim.handle).is_some()
            && active.is_none_or(|a| a.0)
            && if target.atlas.is_some() {
                atlas.locs.contains_key(&entity)
            } else {
                target.image != Handle::default()
            };
        if ready {
            vm.stage_writes();
        }
    }
}

/// M-TEXT analogue of [`stage_vm_writes`]: stages each entity's queued
/// [`RiveText`] writes (`writes` → `staged`) so the read-only extract can ferry
/// them. Same readiness gate — a write for a not-yet-extractable face stays
/// queued in `writes` (retained across the async-load / unallocated / culled
/// window) rather than being stranded in `staged` and silently dropped.
fn stage_text_writes(
    mut query: Query<(
        Entity,
        &RiveAnimation,
        &RiveTarget,
        Option<&RiveActive>,
        &mut RiveText,
    )>,
    files: Res<Assets<RiveFile>>,
    atlas: Res<RiveAtlasState>,
) {
    for (entity, anim, target, active, mut text) in &mut query {
        if !text.has_staging_work() {
            continue;
        }
        // Mirror extract_rive_instances' readiness gate (asset loaded + not culled +
        // surface allocated). Keep in sync with stage_vm_writes / extract.
        let ready = files.get(&anim.handle).is_some()
            && active.is_none_or(|a| a.0)
            && if target.atlas.is_some() {
                atlas.locs.contains_key(&entity)
            } else {
                target.image != Handle::default()
            };
        if ready {
            text.stage_writes();
        }
    }
}

/// Ferries per-entity `Send` data from the main world into the [`ExtractedRives`]
/// render-world resource each frame. Only entities whose `.riv` is loaded *and*
/// whose display image is allocated are included.
///
/// Uses a render-world resource (not per-entity render components) because the
/// rive entities are not synced to the render world; the main-world [`Entity`] is
/// carried as the stable per-instance key.
#[expect(
    clippy::needless_pass_by_value,
    reason = "Bevy Extract systems take Extract<...> by value"
)]
#[expect(
    clippy::type_complexity,
    reason = "Bevy query tuple with an Option<&RiveActive> cull filter"
)]
fn extract_rive_instances(
    mut out: ResMut<ExtractedRives>,
    query: Extract<
        Query<(
            Entity,
            &RiveAnimation,
            &RiveTarget,
            Option<&RiveActive>,
            Option<&RiveViewModel>,
            Option<&RiveFit>,
            Option<&RivePointer>,
            Option<&RiveAssets>,
            Option<&RiveText>,
        )>,
    >,
    files: Extract<Res<Assets<RiveFile>>>,
    atlas: Extract<Res<RiveAtlasState>>,
    time: Extract<Res<Time>>,
) {
    out.items.clear();
    out.generation = out.generation.wrapping_add(1);
    let dt = time.delta_secs();
    for (entity, anim, target, active, vm, fit, pointer, assets, text) in &query {
        let fit_align = fit.copied().unwrap_or_default().fit_align();
        let Some(file) = files.get(&anim.handle) else {
            continue; // not loaded yet — no rive state to keep
        };
        // Culled faces (`RiveActive(false)`) are PAUSED, not despawned: ferry them with no
        // placement + zero step so the node keeps them in the live set (preserving their
        // rive state machine for resume-in-place) but neither advances nor records them.
        // Dropping them here would let `retain` evict their instance → a t=0 reset + a
        // `.riv` re-parse on the next activation (the LOD-cull use case). Their atlas tile
        // is already freed main-world; only the rive state is preserved.
        if active.is_some_and(|a| !a.0) {
            out.items.push(ExtractedRive {
                entity,
                display: Handle::default(),
                bytes: file.bytes.clone(),
                width: target.width,
                height: target.height,
                step: 0.0,
                atlas: None,
                culled: true,
                // Paused: skip advance, so applying writes (which take effect on
                // advance) is pointless this frame — ferry none.
                vm_writes: Vec::new(),
                text_writes: Vec::new(),
                artboard_sel: anim.artboard.clone(),
                state_machine_sel: anim.state_machine.clone(),
                assets: assets.cloned(),
                fit_align,
                // Paused faces are skipped before the node's pointer block, so this
                // is inert; the instance's edge state persists for resume-in-place.
                pointer: None,
                pointer_down: false,
            });
            continue;
        }
        // Atlas faces are gated on an assigned LOC (they get no per-face image); dedicated
        // faces on their display image being allocated.
        let (display, atlas_data) = if target.atlas.is_some() {
            match atlas.locs.get(&entity) {
                Some(&loc) => {
                    let bucket = loc.page.bucket;
                    let atlas_data = ExtractedAtlas {
                        page: loc.page,
                        tile_rect: atlas_tile_rect_px(bucket, loc.slot),
                        page_px: atlas_page_px(bucket),
                        display: atlas.display_of(loc.page),
                    };
                    (Handle::default(), Some(atlas_data))
                }
                None => continue, // slot not assigned yet (or just culled)
            }
        } else {
            if target.image == Handle::default() {
                continue;
            }
            (target.image.clone(), None)
        };
        let step = dt * anim.speed;
        let step = if step.is_finite() { step.max(0.0) } else { 0.0 };
        out.items.push(ExtractedRive {
            entity,
            display,
            bytes: file.bytes.clone(),
            width: target.width,
            height: target.height,
            step,
            atlas: atlas_data,
            culled: false,
            // Ferry this frame's staged view-model writes (read-only extract → the
            // render world, where this tier's instances live). `stage_vm_writes`
            // populated `staged` from `writes` earlier this frame.
            vm_writes: vm.map(|v| v.staged().to_vec()).unwrap_or_default(),
            text_writes: text.map(|t| t.staged().to_vec()).unwrap_or_default(),
            artboard_sel: anim.artboard.clone(),
            state_machine_sel: anim.state_machine.clone(),
            assets: assets.cloned(),
            fit_align,
            // Ferry this frame's pointer (target-pixel space). Absent `RivePointer`
            // or off-surface `pos` both collapse to `None` ⇒ the node fires a single
            // `pointer_exit` (mirrors the floor tier's absent-or-None handling).
            pointer: pointer.and_then(|p| p.pos),
            pointer_down: pointer.is_some_and(|p| p.primary_down),
        });
    }
}

// ===========================================================================
// Render graph node: advance + render into the shared VkImage, copy to display.
// ===========================================================================

/// Render-graph label for [`RiveFillNode`].
#[derive(RenderLabel, Debug, Clone, PartialEq, Eq, Hash)]
struct RiveFillLabel;

/// Render node. Durable rive state lives in [`RiveRenderState`]; this frame's
/// work-list is the [`ExtractedRives`] resource. Stateless — `Node::run` reads
/// both via `&World`. Ordered before `Node2d::StartMainPass` so the display image
/// is filled before any sprite samples it.
#[derive(Default)]
struct RiveFillNode;

impl Node for RiveFillNode {
    fn run<'w>(
        &self,
        _graph: &mut RenderGraphContext,
        render_context: &mut RenderContext<'w>,
        world: &'w World,
    ) -> Result<(), NodeRunError> {
        // Tier is active only once the shared handles have been extracted.
        let Some(handles) = world.get_resource::<RiveSharedHandles>() else {
            return Ok(());
        };
        let Some(state) = world.get_non_send_resource::<RiveRenderState>() else {
            return Ok(());
        };
        let gpu_images = world.resource::<RenderAssets<GpuImage>>();
        let render_device = render_context.render_device().clone();

        // The node is the sole borrower of this RefCell, once per frame, on the
        // main thread (pipelined rendering is disabled for this tier, so the render
        // world runs on the main thread — see RiveRenderState).
        let mut gpu = state.0.borrow_mut();

        // Lazily create the external rive context on wgpu's device.
        if gpu.ctx.is_none() {
            // SAFETY: handles are the live wgpu device's Vulkan handles, valid for
            // the lifetime of the render world; gipa is the device's loader.
            let ctx = unsafe {
                Context::from_wgpu_vulkan(
                    handles.instance,
                    handles.physical_device,
                    handles.device,
                    handles.get_instance_proc_addr as *mut c_void,
                    &handles.features,
                    handles.force_atomic,
                    handles.queue_family_index,
                )
            };
            match ctx {
                Ok(ctx) => gpu.ctx = Some(ctx),
                Err(e) => {
                    error!("rive zero-copy: external context creation failed: {e}");
                    return Ok(());
                }
            }
        }

        // This frame's work-list comes from the extract resource (the rive
        // entities are not render-world-synced, so there are no render entities to
        // query). Clone out so we don't borrow the world across the &mut gpu use.
        let extracted = world.resource::<ExtractedRives>();
        let frame_items: Vec<ExtractedRive> = extracted.items.clone();
        // M-DATA: the per-frame generation gates the once-per-frame VM-write apply (this
        // node can run multiple times per frame — one per camera sub-graph).
        let extracted_gen = extracted.generation;

        gpu.frame = gpu.frame.wrapping_add(1).max(1);
        let frame_no = gpu.frame;
        // M-DATA: apply VM writes exactly once per visual frame. This node runs once per
        // camera sub-graph it is anchored in (twice under the default `Both` with a 2D + a
        // 3D camera), all reading the same `frame_items`; re-applying idempotent setters is
        // harmless, but a `fire_trigger` would pulse once per run. Gate on the extract
        // generation so only the first run of a frame applies.
        let apply_vm_writes = gpu.vm_writes_applied_gen != extracted_gen;
        gpu.vm_writes_applied_gen = extracted_gen;
        let queue = handles.queue;
        // M2a: non-blocking record-into-wgpu by default; RIVE_BLOCKING uses the M1b
        // blocking submit+fence path (needs `queue`).
        let blocking = handles.blocking_submit;
        // SPIKE/Phase-1 (RIVE_BATCH): record ALL instances' artboards in ONE begin/flush
        // (after the loop) to measure the per-flush overhead batching removes. Per-instance
        // record + blit are skipped; only valid on the non-blocking path. RIVE_BATCH=2 adds
        // the per-draw clipRect (the C2 clip-cost A/B vs the no-clip =1 baseline).
        let batch_val = std::env::var("RIVE_BATCH").ok();
        let batch = !blocking && batch_val.is_some();
        let batch_clip = batch_val.as_deref() == Some("2");

        // One-time, now that the context exists: apply the M2.0 perf knobs (the
        // clockwise PLS override + perf-collector config) carried on the handles.
        if !gpu.clockwise_applied {
            if let Some(ctx) = gpu.ctx.as_ref() {
                ctx.set_clockwise(handles.clockwise);
            }
            gpu.perf.enabled = handles.perf_enabled;
            gpu.perf.target = handles.perf_target;
            // Skip the first frames (lazy context/pipeline/instance creation +
            // shader compile) so the summary reflects steady state.
            gpu.perf.warmup = 30;
            gpu.clockwise_applied = true;
        }

        // Split the borrow so we can read `ctx` while mutating `instances`.
        let RiveGpu {
            ctx,
            blit,
            instances,
            atlas_pages,
            atlas_instances,
            logged_mode,
            recycle_unsafe_warned,
            perf,
            ..
        } = &mut *gpu;
        let Some(ctx) = ctx.as_ref() else {
            return Ok(());
        };

        // Lazily build the un-premult display pipeline now that the live
        // RenderDevice exists (it cannot be created during plugin build()).
        let blit = blit.get_or_insert_with(|| RiveBlitPipeline::new(render_device.wgpu_device()));

        // Whether at least one frame actually rendered this call. Gates the
        // one-shot PLS-mode log so it reflects a real frame — `pls_mode()` is only
        // meaningful after a `beginFrame`, and early node calls (before the asset +
        // display image are ready) have an empty work-list.
        let mut rendered_any = false;

        // M2a per-frame perf accumulators (summed across this frame's instances).
        let mut frame_instances = 0u32;
        // M-SCALE: per-phase split (advance | record(=cpu/flush) | blit), so the
        // measure-first question is an observed number, not an assumption.
        let mut frame_advance_us = 0.0_f64;
        let mut frame_cpu_us = 0.0_f64;
        let mut frame_flush_us = 0.0_f64;
        let mut frame_fence_us = 0.0_f64;
        let mut frame_gpu_ms = 0.0_f64;
        let mut frame_blit_us = 0.0_f64;
        // GPU timing is all-or-nothing per frame: if any submit lacked a timestamp,
        // the frame's GPU total is not reported (avoids undercounting).
        let mut frame_gpu_ok = true;

        // M2b: the resource-recycle watermark for this frame, shared by every instance
        // and by both submit paths (so the run-ahead metric reflects the real value).
        // Three cases:
        //  * blocking (M1b): the per-frame fence proves the prior frame finished, so
        //    `frame_no - 1` is exact.
        //  * non-blocking + timeline semaphore (M2b default): read the EXACT highest
        //    frame whose submit has completed on the GPU (non-blocking) — this is what
        //    removes the M2a "≤ ring frames in flight" precondition.
        //  * non-blocking fallback (no timeline semaphore): the fixed ring offset,
        //    sound only under a vsync-bounded surface (see RIVE_RING_SIZE).
        let watermark_sema = vk::Semaphore::from_raw(handles.frame_sync_sema);
        let watermark_active = !blocking && handles.frame_sync_sema != 0;
        let safe_frame = if blocking {
            frame_no.saturating_sub(1)
        } else if watermark_active {
            // SAFETY: `as_hal` yields wgpu's Vulkan device for the call only (guard not
            // stored); reading a timeline semaphore's counter is a non-blocking query
            // with no aliasing. `watermark_sema` is our own live timeline semaphore.
            let completed = unsafe {
                render_device.wgpu_device().as_hal::<Vk>().and_then(|d| {
                    d.raw_device()
                        .get_semaphore_counter_value(watermark_sema)
                        .ok()
                })
            }
            .unwrap_or(0);
            // A frame's own work cannot be complete before it is even submitted.
            completed.min(frame_no.saturating_sub(1))
        } else {
            frame_no.saturating_sub(RIVE_RING_SIZE)
        };

        for item in &frame_items {
            // Culled (paused) faces: skip advance/record/instantiate. They stay in `live`
            // (built from frame_items below), so `retain` KEEPS their rive state for a
            // resume-in-place — we just don't tick or draw them this frame.
            if item.culled {
                continue;
            }
            // Atlas-opted faces (RiveTarget.atlas = Some) render together in the atlas
            // block after this loop — never per-instance (they have no own image).
            if item.atlas.is_some() {
                continue;
            }
            let entity = item.entity;
            // Instantiate native objects + shared texture on first sight. Building
            // is fallible; on error we skip this entity for the frame. The Vacant
            // entry avoids a redundant contains_key + insert double-lookup.
            if let std::collections::hash_map::Entry::Vacant(slot) = instances.entry(entity) {
                match build_instance(ctx, &render_device, item) {
                    Ok(inst) => {
                        slot.insert(inst);
                    }
                    Err(e) => {
                        warn!("rive zero-copy: instantiate {entity:?} failed: {e}");
                        continue;
                    }
                }
            }
            let Some(inst) = instances.get_mut(&entity) else {
                continue;
            };

            // Apply this face's fit/alignment to the artboard before drawing (the
            // dedicated path's analogue of the floor advance system). Absent
            // `RiveFit` == contain/center, so this is a no-op for most faces.
            inst.artboard.set_fit_align(item.fit_align);
            // The pointer inversion runs through the state machine's fit/alignment,
            // so it must match the artboard's (above) for hits to track the pixels.
            inst.state_machine.set_fit_align(item.fit_align);

            // Forward pointer input to the state machine's Listeners BEFORE advance
            // (the dedicated-path analogue of the floor advance system): a listener
            // latches the target, then the joystick eases toward it on `advance` — so
            // re-assert `pointer_move` every frame while present, emitting press/
            // release/exit only on edges (see `RivePointer`). Dedicated path only;
            // the atlas path's tile-rect draw would need tile-aware coords (deferred).
            let (pw, ph) = (item.width, item.height);
            match item.pointer.map(|pos| (pos, item.pointer_down)) {
                Some((pos, down)) => {
                    inst.state_machine.pointer_move(pos.x, pos.y, pw, ph);
                    if down && !inst.last_pointer_down {
                        inst.state_machine.pointer_down(pos.x, pos.y, pw, ph);
                    } else if !down && inst.last_pointer_down {
                        inst.state_machine.pointer_up(pos.x, pos.y, pw, ph);
                    }
                    inst.last_pointer_down = down;
                    inst.last_pointer_present = true;
                }
                None => {
                    if inst.last_pointer_present {
                        // The exit position is ignored by the SM; (0,0) is fine.
                        inst.state_machine.pointer_exit(0.0, 0.0, pw, ph);
                    }
                    inst.last_pointer_down = false;
                    inst.last_pointer_present = false;
                }
            }

            // Advance the state machine, then run rive's frame. Two paths:
            //  * M2a non-blocking (default): RECORD rive's draws into wgpu's OWN open
            //    command buffer (no submit, no fence) — rive's work rides wgpu's
            //    per-frame submit, GPU-ordered before the un-premult pass below.
            //  * M1b blocking (RIVE_BLOCKING=1): out-of-band submit + blocking fence,
            //    kept as a selectable fallback and an A/B baseline on one build.
            // M-DATA: apply this frame's view-model writes before advancing, so the
            // state machine / scripts observe them this tick (the floor advance
            // system's inline apply, ferried here for the render-world tier). Gated to
            // once per visual frame (see `apply_vm_writes`) so a `fire_trigger` pulses once.
            if apply_vm_writes && !item.vm_writes.is_empty() {
                crate::view_model::apply_writes_slice(&inst.artboard, &item.vm_writes);
            }
            // M-TEXT: apply this frame's text-run set writes before advance too (same
            // once-per-frame gate; text sets are idempotent so the gate is just an opt).
            if apply_vm_writes && !item.text_writes.is_empty() {
                crate::text::apply_text_writes_slice(&inst.artboard, &item.text_writes);
            }
            // M-SCALE: time the per-instance state-machine tick (previously untimed
            // — it runs before the record span, so the collector never saw it).
            let advance_t0 = std::time::Instant::now();
            inst.state_machine.advance(item.step);
            frame_advance_us += advance_t0.elapsed().as_secs_f64() * 1.0e6;

            // SPIKE: under RIVE_BATCH the per-instance record + blit below are skipped;
            // all artboards are recorded together in one begin/flush after the loop.
            if batch {
                continue;
            }

            // For the non-blocking path, fetch wgpu's open primary command buffer for
            // this frame; rive records into it via its own Vulkan dispatch.
            let cmd_buffer = if blocking {
                0
            } else {
                // SAFETY: `as_hal_mut` is `unsafe`; `raw_handle()` returns wgpu's open
                // primary buffer for this frame. We read the handle only and do not
                // end the buffer or touch the wgpu encoder until rive's record
                // returns, per the `as_hal_mut` contract.
                unsafe {
                    render_context
                        .command_encoder()
                        .as_hal_mut::<Vk, _, _>(|enc| enc.map(|e| e.raw_handle().as_raw()))
                }
                .unwrap_or(0)
            };
            if !blocking && cmd_buffer == 0 {
                warn!("rive zero-copy: wgpu encoder is not Vulkan; cannot record {entity:?}");
                continue;
            }

            // CPU-time the rive call. Non-blocking: rive's CPU flush/record only (the
            // blocking-fence stall is gone). Blocking: flush + submit + the blocking
            // fence wait (the M1b / Step-0 baseline). The shim's flush/fence sub-span
            // timers (read just below) attribute the wall.
            let submit_t0 = std::time::Instant::now();
            let submit_res = if blocking {
                let submit = ExternalFrameSubmit {
                    current_frame: frame_no,
                    // blocking case of the unified watermark above (= frame_no - 1).
                    safe_frame,
                    queue,
                };
                // SAFETY: `queue` is wgpu's graphics VkQueue on this context's device;
                // the node runs serialized on the render thread (pipelining disabled),
                // before StartMainPass.
                unsafe {
                    ctx.render_external_frame(
                        &inst.target,
                        &inst.artboard,
                        crate::rive_clear_rgba(),
                        submit,
                    )
                }
            } else {
                let record = ExternalFrameRecord {
                    current_frame: frame_no,
                    // M2b: the exact GPU-completion watermark (or fixed-ring fallback),
                    // computed once above for the whole frame.
                    safe_frame,
                    command_buffer: cmd_buffer,
                };
                // SAFETY: `cmd_buffer` is wgpu's open buffer on this context's device;
                // rive leaves the image in SHADER_READ_ONLY == wgpu's RESOURCE layout;
                // the node is before StartMainPass on the render thread.
                unsafe {
                    ctx.record_external_frame(
                        &inst.target,
                        &inst.artboard,
                        crate::rive_clear_rgba(),
                        record,
                    )
                }
            };
            let submit_cpu_us = submit_t0.elapsed().as_secs_f64() * 1.0e6;
            if let Err(e) = submit_res {
                warn!("rive zero-copy: frame {entity:?} failed: {e}");
                continue;
            }
            // A frame's beginFrame + flush ran, so `pls_mode()` is now meaningful
            // (it is captured at beginFrame). Gate the one-shot PLS log on this.
            rendered_any = true;
            // M2a: accumulate this submit into the per-frame perf totals. flush +
            // fence-wait are the shim's CPU sub-span timers (the fence-vs-flush
            // split); gpu_ms is rive's command-buffer time (Vulkan timestamps).
            // Summed across instances and recorded once per frame below.
            frame_instances += 1;
            frame_cpu_us += submit_cpu_us;
            frame_flush_us += ctx.last_flush_us().unwrap_or(0.0);
            frame_fence_us += ctx.last_fence_wait_us().unwrap_or(0.0);
            match ctx.last_gpu_ms() {
                Some(ms) => frame_gpu_ms += ms,
                None => frame_gpu_ok = false,
            }

            // DISPLAY: un-premultiply + sRGB-decode fullscreen pass from the shared
            // Rgba8Unorm texture (rive's premultiplied, sRGB-encoded bytes) into the
            // Rgba8UnormSrgb display image the Sprite samples. Correct for BOTH
            // opaque and transparent content; matches M1a's straight-alpha display
            // (design spec §7 Option B).
            let Some(dst) = gpu_images.get(&inst.display) else {
                // One-time diagnostic: the display GpuImage isn't prepared yet, so
                // the pass is skipped and the sprite stays blank this frame.
                if !*logged_mode {
                    warn!(
                        "rive zero-copy: display GpuImage NOT ready for {entity:?} — pass skipped this frame"
                    );
                }
                continue;
            };
            if !*logged_mode {
                info!(
                    "rive zero-copy: un-premult pass recorded shared->display for {entity:?} (GpuImage ready)"
                );
            }

            // Bind the shared texture as the sole source (no sampler — the WGSL
            // uses textureLoad), then draw a fullscreen triangle into the display
            // texture view. REPLACE blend (pipeline `blend: None`) overwrites the
            // whole display image.
            // M-SCALE: time the un-premult blit-encode (bind-group lookup + pass) so
            // the per-phase split (advance | record | blit) is observable. The bind
            // group is cached on the instance — its only input, `shared_view`, is a
            // stable zero-copy texture — so steady-state frames re-encode the pass
            // only, with no per-frame `create_bind_group`.
            let blit_t0 = std::time::Instant::now();
            let device = render_device.wgpu_device();
            if inst.bind_group.is_none() {
                let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("rive_unpremult_bg"),
                    layout: &blit.layout,
                    entries: &[wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&inst.shared_view),
                    }],
                });
                inst.bind_group = Some(bg);
            }
            let bind_group = inst.bind_group.as_ref().unwrap();
            let color_attachments = [Some(wgpu::RenderPassColorAttachment {
                view: &dst.texture_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })];
            {
                let mut pass = render_context.command_encoder().begin_render_pass(
                    &wgpu::RenderPassDescriptor {
                        label: Some("rive_unpremult_pass"),
                        color_attachments: &color_attachments,
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                    },
                );
                pass.set_pipeline(&blit.pipeline);
                pass.set_bind_group(0, bind_group, &[]);
                pass.draw(0..3, 0..1);
            }
            frame_blit_us += blit_t0.elapsed().as_secs_f64() * 1.0e6;
        }

        // SPIKE (RIVE_BATCH): one begin -> N draws -> one record for the WHOLE frame,
        // to measure the per-flush overhead that real (atlas) batching would remove.
        // All artboards overlap into the first instance's target — this times CPU
        // record only (no display blit under batch).
        if batch {
            let built: Vec<&RiveInstance> = frame_items
                .iter()
                .filter_map(|it| instances.get(&it.entity))
                .collect();
            if let Some(first) = built.first() {
                // wgpu's single open primary command buffer for this frame.
                // SAFETY: `as_hal_mut` yields wgpu's open buffer; we read the handle only.
                let cmd_buffer = unsafe {
                    render_context
                        .command_encoder()
                        .as_hal_mut::<Vk, _, _>(|enc| enc.map(|e| e.raw_handle().as_raw()))
                }
                .unwrap_or(0);
                if cmd_buffer == 0 {
                    warn!("rive zero-copy: batch path — wgpu encoder is not Vulkan");
                } else {
                    let artboards: Vec<&Artboard> = built.iter().map(|i| &i.artboard).collect();
                    let record = ExternalFrameRecord {
                        current_frame: frame_no,
                        safe_frame,
                        command_buffer: cmd_buffer,
                    };
                    let t0 = std::time::Instant::now();
                    // SAFETY: `cmd_buffer` is wgpu's open buffer on this context's device;
                    // the node runs serialized on the render thread before StartMainPass.
                    let res = unsafe {
                        ctx.record_external_frame_batched(
                            &first.target,
                            &artboards,
                            crate::rive_clear_rgba(),
                            record,
                            batch_clip,
                        )
                    };
                    frame_cpu_us += t0.elapsed().as_secs_f64() * 1.0e6;
                    match res {
                        Ok(()) => {
                            rendered_any = true;
                            frame_instances = built.len() as u32;
                            frame_flush_us += ctx.last_flush_us().unwrap_or(0.0);
                        }
                        Err(e) => warn!("rive zero-copy: batched record failed: {e}"),
                    }
                }
            }
        }

        // M-SCALE atlas path (multi-page, per-LOD bucket): render every ACTIVE atlas-opted
        // face into its PAGE — one begin/flush per page (writing DISTINCT, gutter-inset
        // tiles), then ONE un-premult pass per page into that page's straight-alpha display
        // image (the consumer samples its tile). Page count is O(pages) ≪ O(faces); each
        // page reproduces the Phase-2 single-flush record win.
        let any_atlas = !blocking && frame_items.iter().any(|i| i.atlas.is_some());
        if any_atlas {
            // Instantiate (artboard + state machine) any new atlas faces, then advance all
            // (advance is page-independent — one pass over every atlas face this frame).
            for item in &frame_items {
                if item.atlas.is_none() {
                    continue;
                }
                if let std::collections::hash_map::Entry::Vacant(e) =
                    atlas_instances.entry(item.entity)
                {
                    match build_atlas_instance(ctx, item) {
                        Ok(inst) => {
                            e.insert(inst);
                        }
                        Err(err) => {
                            warn!("rive zero-copy: atlas instance {:?} failed: {err}", item.entity);
                        }
                    }
                }
            }
            for item in &frame_items {
                let Some(atlas) = &item.atlas else {
                    continue; // dedicated or culled face (no tile this frame)
                };
                if let Some(inst) = atlas_instances.get_mut(&item.entity) {
                    // M-DATA: apply view-model writes before advance (see the
                    // dedicated-path apply above), gated once per visual frame.
                    if apply_vm_writes && !item.vm_writes.is_empty() {
                        crate::view_model::apply_writes_slice(&inst.artboard, &item.vm_writes);
                    }
                    // M-TEXT: apply text-run set writes before advance (see dedicated path).
                    if apply_vm_writes && !item.text_writes.is_empty() {
                        crate::text::apply_text_writes_slice(&inst.artboard, &item.text_writes);
                    }
                    // Fit/alignment within the tile (draw_viewport reads it). Absent
                    // `RiveFit` == contain/center — the historical per-tile fit.
                    inst.artboard.set_fit_align(item.fit_align);

                    // Pointer input → Listeners, TILE-AWARE. An atlas face is fit into
                    // its tile sub-rect (draw_viewport), not the full target, so the
                    // inversion needs BOTH the same fit/alignment AND the drawn tile
                    // size: target-space coords (`0..width`, `0..height`) are normalized
                    // into the tile before the fit/alignment is inverted. `tile_rect` is
                    // `[x, y, w, h]` in page px; only the drawn `w`×`h` matters (the tile
                    // offset cancels under computeAlignment). Forward BEFORE advance and
                    // re-assert every frame, emitting press/release/exit on edges —
                    // exactly the dedicated path, just with the tile mapping set.
                    inst.state_machine.set_fit_align(item.fit_align);
                    inst.state_machine
                        .set_pointer_tile(atlas.tile_rect[2], atlas.tile_rect[3]);
                    let (pw, ph) = (item.width, item.height);
                    match item.pointer.map(|pos| (pos, item.pointer_down)) {
                        Some((pos, down)) => {
                            inst.state_machine.pointer_move(pos.x, pos.y, pw, ph);
                            if down && !inst.last_pointer_down {
                                inst.state_machine.pointer_down(pos.x, pos.y, pw, ph);
                            } else if !down && inst.last_pointer_down {
                                inst.state_machine.pointer_up(pos.x, pos.y, pw, ph);
                            }
                            inst.last_pointer_down = down;
                            inst.last_pointer_present = true;
                        }
                        None => {
                            if inst.last_pointer_present {
                                // The exit position is ignored by the SM; (0,0) is fine.
                                inst.state_machine.pointer_exit(0.0, 0.0, pw, ph);
                            }
                            inst.last_pointer_down = false;
                            inst.last_pointer_present = false;
                        }
                    }

                    let advance_t0 = std::time::Instant::now();
                    inst.state_machine.advance(item.step);
                    frame_advance_us += advance_t0.elapsed().as_secs_f64() * 1.0e6;
                }
            }

            // Group active atlas faces by page (each page = one begin/flush + one blit).
            let mut groups: HashMap<AtlasPageId, Vec<usize>> = HashMap::new();
            for (i, item) in frame_items.iter().enumerate() {
                if let Some(a) = &item.atlas {
                    groups.entry(a.page).or_default().push(i);
                }
            }

            // wgpu's open primary command buffer (stable for the frame); rive records into it.
            // SAFETY: as_hal_mut yields wgpu's open buffer; we read the handle only.
            let cmd_buffer = unsafe {
                render_context
                    .command_encoder()
                    .as_hal_mut::<Vk, _, _>(|enc| enc.map(|e| e.raw_handle().as_raw()))
            }
            .unwrap_or(0);
            if cmd_buffer == 0 {
                warn!("rive zero-copy: atlas path — wgpu encoder is not Vulkan");
            } else {
                for (page_id, idxs) in &groups {
                    let Some(page_px) = frame_items[idxs[0]].atlas.as_ref().map(|a| a.page_px)
                    else {
                        continue;
                    };
                    // Ensure the render-world page texture exists (sized to this bucket's page).
                    if !atlas_pages.contains_key(page_id) {
                        match build_atlas(ctx, &render_device, page_px) {
                            Ok(a) => {
                                atlas_pages.insert(*page_id, a);
                            }
                            Err(e) => {
                                warn!("rive zero-copy: atlas page {page_id:?} build failed: {e}");
                                continue;
                            }
                        }
                    }
                    let page = atlas_pages.get_mut(page_id).unwrap();

                    // (artboard, gutter-inset tile rect) for this page — MAIN-assigned, so the
                    // rendered tiles match the `uv_rect` each consumer was handed.
                    let tiles: Vec<(&Artboard, [f32; 4])> = idxs
                        .iter()
                        .filter_map(|&i| {
                            let a = frame_items[i].atlas.as_ref()?;
                            let inst = atlas_instances.get(&frame_items[i].entity)?;
                            Some((&inst.artboard, a.tile_rect))
                        })
                        .collect();
                    if tiles.is_empty() {
                        continue;
                    }
                    let record = ExternalFrameRecord {
                        current_frame: frame_no,
                        safe_frame,
                        command_buffer: cmd_buffer,
                    };
                    let t0 = std::time::Instant::now();
                    // SAFETY: cmd_buffer is wgpu's open buffer on this context's device; the
                    // node runs serialized on the render thread before StartMainPass.
                    let res = unsafe {
                        ctx.record_external_atlas_frame(
                            &page.target,
                            &tiles,
                            crate::rive_clear_rgba(),
                            record,
                        )
                    };
                    frame_cpu_us += t0.elapsed().as_secs_f64() * 1.0e6;
                    match res {
                        Ok(()) => {
                            rendered_any = true;
                            frame_instances += tiles.len() as u32;
                            frame_flush_us += ctx.last_flush_us().unwrap_or(0.0);
                        }
                        Err(e) => {
                            warn!("rive zero-copy: atlas record (page {page_id:?}) failed: {e}");
                            continue;
                        }
                    }

                    // ONE un-premult pass for this page: premultiplied page -> the straight-
                    // alpha display page the consumers sample. dst-pixel == src-pixel (both
                    // page_px²), so UNPREMULT_WGSL is unchanged; bind group cached per page.
                    let display = &frame_items[idxs[0]].atlas.as_ref().unwrap().display;
                    if let Some(dst) = gpu_images.get(display) {
                        let blit_t0 = std::time::Instant::now();
                        let device = render_device.wgpu_device();
                        if page.bind_group.is_none() {
                            let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                                label: Some("rive_atlas_unpremult_bg"),
                                layout: &blit.layout,
                                entries: &[wgpu::BindGroupEntry {
                                    binding: 0,
                                    resource: wgpu::BindingResource::TextureView(&page.shared_view),
                                }],
                            });
                            page.bind_group = Some(bg);
                        }
                        let bind_group = page.bind_group.as_ref().unwrap();
                        let color_attachments = [Some(wgpu::RenderPassColorAttachment {
                            view: &dst.texture_view,
                            depth_slice: None,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                                store: wgpu::StoreOp::Store,
                            },
                        })];
                        {
                            let mut pass = render_context.command_encoder().begin_render_pass(
                                &wgpu::RenderPassDescriptor {
                                    label: Some("rive_atlas_unpremult_pass"),
                                    color_attachments: &color_attachments,
                                    depth_stencil_attachment: None,
                                    timestamp_writes: None,
                                    occlusion_query_set: None,
                                },
                            );
                            pass.set_pipeline(&blit.pipeline);
                            pass.set_bind_group(0, bind_group, &[]);
                            pass.draw(0..3, 0..1);
                        }
                        frame_blit_us += blit_t0.elapsed().as_secs_f64() * 1.0e6;
                    }
                }
            }
        }

        // M2a: record this frame's aggregate perf (summed over instances), only
        // when work actually ran — so warm-up / `seen` count real frames, not the
        // empty early node calls before the asset + display image are ready.
        if frame_instances > 0 {
            // run-ahead = frames submitted but not yet GPU-complete (current - safe).
            let run_ahead = frame_no.saturating_sub(safe_frame) as f64;
            perf.record_frame(FrameTimings {
                instances: frame_instances,
                advance_us: frame_advance_us,
                cpu_us: frame_cpu_us,
                flush_us: frame_flush_us,
                fence_us: frame_fence_us,
                gpu_ms: frame_gpu_ok.then_some(frame_gpu_ms),
                blit_us: frame_blit_us,
                run_ahead,
            });
        }

        // Log the active interlock mode once a real frame has rendered (so
        // `pls_mode()` reflects a captured `beginFrame`, not an empty early call).
        // M2.0: also report the clockwise request and whether GPU timing is live,
        // so a perf run's log is self-describing.
        if rendered_any && !*logged_mode {
            info!(
                "rive zero-copy: PLS mode = {:?}, raster-order supported = {}, \
                 clockwise requested = {}, sync = {}, GPU timing = {}",
                ctx.pls_mode(),
                ctx.supports_raster_ordering(),
                handles.clockwise,
                if blocking {
                    "blocking (M1b submit+fence)"
                } else if watermark_active {
                    "non-blocking + timeline-semaphore watermark (M2b, exact)"
                } else {
                    "non-blocking + fixed ring-offset watermark (M2a fallback)"
                },
                if ctx.last_gpu_ms().is_some() {
                    "available"
                } else {
                    "unavailable"
                },
            );
            *logged_mode = true;
        }

        // M2b sync guard — evaluated EVERY frame (not latched to the one-shot PLS log
        // above), so a runtime present-mode change INTO the unsafe regime still warns.
        // Only the FIXED-RING fallback has a precondition: the exact timeline-semaphore
        // watermark proves GPU completion regardless of frames-in-flight, so when it is
        // active there is nothing to warn about. In the fallback, `safe_frame = current -
        // RIVE_RING_SIZE` is sound only while frames-in-flight ≤ RIVE_RING_SIZE; warn when
        // the live window config could exceed that (non-vsync present mode, or latency
        // past the ring) — else rive could recycle a pooled buffer the GPU still reads
        // (silent corruption). RIVE_BLOCKING=1 is the safe fallback. See RIVE_RING_SIZE.
        if !blocking && !watermark_active {
            let unsafe_now = world
                .get_resource::<ExtractedWindows>()
                .is_some_and(|windows| {
                    windows.windows.values().any(|w| {
                        let vsync = matches!(
                            w.present_mode,
                            PresentMode::Fifo | PresentMode::FifoRelaxed | PresentMode::AutoVsync
                        );
                        let latency = w.desired_maximum_frame_latency.map_or(2, |n| n.get());
                        let in_flight = if vsync {
                            u64::from(latency) + 1
                        } else {
                            u64::MAX
                        };
                        in_flight > RIVE_RING_SIZE
                    })
                });
            // Warn once per entry into the unsafe regime; reset when safe again so a later
            // transition re-warns.
            if unsafe_now && !*recycle_unsafe_warned {
                warn!(
                    "rive zero-copy: non-blocking sync (fixed-ring fallback) assumes ≤ {RIVE_RING_SIZE} \
                     frames in flight, but the window present mode / desired_maximum_frame_latency may \
                     exceed it — rive can recycle a pooled buffer the GPU is still reading (silent \
                     corruption). Use PresentMode::Fifo with latency ≤ {}, or RIVE_BLOCKING=1.",
                    RIVE_RING_SIZE - 1,
                );
            }
            *recycle_unsafe_warned = unsafe_now;
        }

        // Drop instances whose entity is no longer extracted this frame.
        let live: std::collections::HashSet<Entity> =
            frame_items.iter().map(|x| x.entity).collect();
        instances.retain(|e, _| live.contains(e));
        atlas_instances.retain(|e, _| live.contains(e));

        // M2b: arm this frame's GPU-completion signal. `add_signal_semaphore` appends
        // (sema, frame_no) to wgpu-hal's per-queue signal list, which THIS frame's
        // single graph submit drains (the render graph records every node, then submits
        // exactly once — so nothing else submits in between). The timeline therefore
        // reaches `frame_no` precisely when this frame's recorded rive work completes on
        // the GPU; a later frame reads that as the exact `safe_frame` above. `frame_no`
        // is monotonic, satisfying the timeline rule that signalled values increase.
        if watermark_active {
            // SAFETY: the render queue is wgpu's graphics VkQueue on this device; we
            // only append a signal value (interior-mutex) and never submit or own it.
            // The hal guard is dropped immediately and never stored.
            if let Some(q) = unsafe { world.resource::<RenderQueue>().as_hal::<Vk>() } {
                q.add_signal_semaphore(watermark_sema, Some(frame_no));
            }
        }

        Ok(())
    }
}

/// Builds one entity's native rive objects + the shared wgpu texture, and wraps
/// the texture's `VkImage` as rive's zero-copy render target.
fn build_instance(
    ctx: &Context,
    render_device: &RenderDevice,
    item: &ExtractedRive,
) -> rive_renderer::Result<RiveInstance> {
    let file = crate::assets::load_file_with_assets(ctx, &item.bytes, item.assets.as_ref())?;
    let artboard = crate::select_artboard(&file, &item.artboard_sel)?;
    let state_machine = crate::select_state_machine(&artboard, &item.state_machine_sel)?;

    // wgpu allocates the shared color texture; rive renders into its VkImage.
    let shared_tex = render_device.create_texture(&TextureDescriptor {
        label: Some("rive_shared_target"),
        size: Extent3d {
            width: item.width,
            height: item.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format: SHARED_FORMAT,
        usage: TextureUsages::RENDER_ATTACHMENT
            | TextureUsages::TEXTURE_BINDING
            | TextureUsages::COPY_DST
            | TextureUsages::COPY_SRC,
        view_formats: &[],
    });

    // Extract the VkImage from the wgpu texture (guard form; copy the handle out).
    // SAFETY: the guard is held only for the extraction; `shared_tex` (kept in the
    // returned instance) owns the texture, so the VkImage stays valid.
    let vk_image = unsafe {
        let g = shared_tex.as_hal::<Vk>().ok_or_else(|| {
            rive_renderer::Error::ContextCreation("shared texture not Vulkan".into())
        })?;
        g.raw_handle().as_raw()
    };

    // Wrap it as rive's render target (shim creates a matching VkImageView when
    // we pass view == 0).
    // SAFETY: `vk_image` is a live wgpu texture's VkImage on this context's device,
    // of the given format/usage, kept alive by `shared_tex` in the instance.
    let target = unsafe {
        ctx.wrap_vk_image(
            vk_image,
            0,
            item.width,
            item.height,
            VK_FORMAT_R8G8B8A8_UNORM,
            RIVE_TARGET_VK_USAGE,
        )?
    };

    // A sampled view of the shared texture for the un-premult pass's bind group.
    let shared_view = shared_tex.create_view(&wgpu::TextureViewDescriptor::default());

    Ok(RiveInstance {
        artboard,
        state_machine,
        target,
        shared_tex,
        shared_view,
        bind_group: None,
        display: item.display.clone(),
        last_pointer_down: false,
        last_pointer_present: false,
    })
}

/// Builds one atlas page's shared `page_px`×`page_px` wgpu texture and wraps its `VkImage`
/// as a single rive render target — [`build_instance`]'s texture path, but allocated ONCE
/// for the whole page. `page_px` comes from the face's LOD bucket ([`atlas_page_px`]).
fn build_atlas(
    ctx: &Context,
    render_device: &RenderDevice,
    page_px: u32,
) -> rive_renderer::Result<RiveAtlas> {
    let (w, h) = (page_px, page_px);
    let shared_tex = render_device.create_texture(&TextureDescriptor {
        label: Some("rive_atlas_shared"),
        size: Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format: SHARED_FORMAT,
        usage: TextureUsages::RENDER_ATTACHMENT
            | TextureUsages::TEXTURE_BINDING
            | TextureUsages::COPY_DST
            | TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    // SAFETY: the guard is held only for the extraction; `shared_tex` (kept in the
    // returned atlas) owns the texture, so the VkImage stays valid.
    let vk_image = unsafe {
        let g = shared_tex.as_hal::<Vk>().ok_or_else(|| {
            rive_renderer::Error::ContextCreation("atlas texture not Vulkan".into())
        })?;
        g.raw_handle().as_raw()
    };
    // SAFETY: `vk_image` is a live wgpu texture's VkImage on this context's device,
    // kept alive by `shared_tex` in the returned atlas.
    let target = unsafe {
        ctx.wrap_vk_image(vk_image, 0, w, h, VK_FORMAT_R8G8B8A8_UNORM, RIVE_TARGET_VK_USAGE)?
    };
    let shared_view = shared_tex.create_view(&wgpu::TextureViewDescriptor::default());
    Ok(RiveAtlas {
        target,
        shared_tex,
        shared_view,
        bind_group: None,
    })
}

/// Builds the atlas-path rive state for one entity: load the file + instantiate the
/// default artboard and state machine. No per-instance texture (the atlas is the target).
fn build_atlas_instance(
    ctx: &Context,
    item: &ExtractedRive,
) -> rive_renderer::Result<AtlasInstance> {
    let file = crate::assets::load_file_with_assets(ctx, &item.bytes, item.assets.as_ref())?;
    let artboard = crate::select_artboard(&file, &item.artboard_sel)?;
    let state_machine = crate::select_state_machine(&artboard, &item.state_machine_sel)?;
    Ok(AtlasInstance {
        artboard,
        state_machine,
        last_pointer_down: false,
        last_pointer_present: false,
    })
}
