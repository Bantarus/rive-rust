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
//!    `Node2d::StartMainPass`) lazily builds rive's context + per-entity
//!    instances, advances each state machine, renders it into the **shared**
//!    `Rgba8Unorm` texture out-of-band (rive submits its own command buffer; the
//!    shim fences), then copies that texture into the **display**
//!    `Rgba8UnormSrgb` `Image` the `Sprite` samples.
//!
//! # Display (step 1 vs the un-premultiply pass)
//!
//! M1b renders rive's *premultiplied* bytes into the shared `Rgba8Unorm` texture
//! and currently copies them straight into the `Rgba8UnormSrgb` display image
//! (`copy_texture_to_texture` — byte-identical, copy-compatible). The `Sprite`
//! then hardware-sRGB-decodes on sample. This is **pixel-correct for opaque
//! content** (the M0/M1.0 references, where premultiplied == straight) and is the
//! M1b step-1 display. Fully-correct *transparent* compositing needs the
//! un-premultiply + sRGB-decode fullscreen pass (design spec §7 Option B); that
//! is the documented follow-up and is flagged at the copy site.

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
use bevy::render::renderer::{RenderContext, RenderDevice};
use bevy::render::texture::GpuImage;
use bevy::render::{Extract, RenderApp};

use bevy::core_pipeline::core_2d::graph::{Core2d, Node2d};
use bevy::render::renderer::raw_vulkan_init::{AdditionalVulkanFeatures, RawVulkanInitSettings};
use bevy::render::renderer::{RenderAdapter, RenderInstance};
use bevy::render::view::ExtractedWindows;
use bevy::window::PresentMode;

use rive_renderer::{
    Artboard, Context, ExternalFrameRecord, ExternalFrameSubmit, RenderTarget, StateMachine,
    VulkanFeatures,
};

use crate::{RiveAnimation, RiveFile, RivePlugin, RiveTarget};

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

/// The display texture behind `RiveTarget.image`: `Rgba8UnormSrgb` straight-alpha,
/// **identical to the M1a seam**, so the user's `Sprite` path is unchanged.
const DISPLAY_FORMAT: TextureFormat = TextureFormat::Rgba8UnormSrgb;

/// rive's transient-resource ring depth (`gpu::kBufferRingSize`). In the M2a
/// non-blocking path there is no fence proving GPU completion, so `safe_frame =
/// current - RIVE_RING_SIZE` is the ONLY signal telling rive a frame's pooled,
/// host-mapped buffers are safe to recycle (rive's `acquire()` reuses a buffer iff
/// its `lastFrameNumber <= safeFrameNumber`, then a CPU memcpy rewrites it). This is
/// correct **only while frames-in-flight ≤ RIVE_RING_SIZE**: if the CPU outruns GPU
/// completion by more than the ring, rive overwrites a buffer the GPU is still
/// reading → silent content corruption. Bevy's default surface (Fifo / AutoVsync,
/// `desired_maximum_frame_latency` 2 → a 3-image swapchain) caps the CPU at ~3
/// frames ahead, matching the ring — so the default is safe, and that is what every
/// M2a measurement ran under. Non-Fifo present modes (Immediate / Mailbox /
/// AutoNoVsync) or a higher frame latency break the bound; the node emits a one-shot
/// warning when it detects such a config, and `RIVE_BLOCKING=1` is the safe fallback.
/// The robust fix — derive the watermark from wgpu's completed `SubmissionIndex` via
/// `device.poll`, instead of a fixed offset — is M2 remainder. Must match rive's
/// `kBufferRingSize`.
const RIVE_RING_SIZE: u64 = 3;

// ===========================================================================
// Plugin.
// ===========================================================================

/// The M1b zero-copy plugin. Registers the `.riv` asset + loader (via the shared
/// [`RivePlugin`] machinery is *not* reused — see below), the main-world display
/// allocation system, the render-world extract + handle systems, and the
/// [`RiveFillNode`] render-graph node.
///
/// Wiring (see the `sprite_riv_zerocopy` example):
/// ```ignore
/// let mut app = App::new();
/// bevy_rive::install_interlock_device_callback(&mut app); // BEFORE DefaultPlugins
/// app.add_plugins(DefaultPlugins);
/// app.add_plugins(RiveZeroCopyPlugin);                    // INSTEAD of RivePlugin
/// ```
///
/// `RiveZeroCopyPlugin` registers the asset + loader itself (so it composes
/// without the M1a CPU systems double-driving the same entities). It does *not*
/// add the M1a `NonSend` systems; M1b entities are driven entirely in the render
/// world.
#[derive(Debug, Default)]
pub struct RiveZeroCopyPlugin;

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

        // Main world: allocate the display Image (the frozen seam) once the .riv
        // has loaded. This is the ONLY main-world M1b work.
        app.add_systems(Update, allocate_display_images);

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

        // The fill node, ordered before all 2D sampling (StartMainPass precedes
        // the opaque + transparent sprite passes).
        render_app
            .add_render_graph_node::<RiveFillNode>(Core2d, RiveFillLabel)
            .add_render_graph_edges(Core2d, (RiveFillLabel, Node2d::StartMainPass));
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
    /// Force rive's atomic PLS path (`RIVE_FORCE_ATOMIC` env). Needed on devices
    /// that *advertise* `VK_EXT_fragment_shader_interlock` but cannot execute it
    /// — e.g. WSL2's Mesa Dozen (Vulkan→D3D12), where the interlock path is
    /// `VK_ERROR_DEVICE_LOST` on submit. Native NVIDIA Vulkan runs interlock fine,
    /// so this is a dev/test escape hatch, not the production path.
    force_atomic: bool,
    /// M2.0 perf lever (`RIVE_CLOCKWISE` env): opt into rive's per-frame
    /// `clockwiseFillOverride`. On desktop NVIDIA (no raster-order ext) the default
    /// path is atomics; this asks for the clockwise path instead, for an A/B.
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
    /// Per-frame rive submit wall, summed over the frame's instances.
    frame_cpu_us: Vec<f64>,
    /// Per-frame rive CPU `flush()` total (shim-measured), summed over instances.
    frame_flush_us: Vec<f64>,
    /// Per-frame blocking fence-wait total (shim-measured), summed over instances.
    frame_fence_us: Vec<f64>,
    /// Per-frame rive GPU command-buffer total, summed over instances.
    frame_gpu_ms: Vec<f64>,
    summarized: bool,
}

impl PerfStats {
    /// Record one frame's aggregate timings (summed over `instances` submits).
    /// `gpu_ms` is `None` if GPU timing was unavailable for any submit this frame.
    fn record_frame(
        &mut self,
        instances: u32,
        cpu_us: f64,
        flush_us: f64,
        fence_us: f64,
        gpu_ms: Option<f64>,
    ) {
        if !self.enabled || self.summarized {
            return;
        }
        self.seen += 1;
        if self.seen <= self.warmup {
            return;
        }
        self.instances = instances;
        self.frame_cpu_us.push(cpu_us);
        self.frame_flush_us.push(flush_us);
        self.frame_fence_us.push(fence_us);
        if let Some(ms) = gpu_ms {
            self.frame_gpu_ms.push(ms);
        }
        if self.frame_cpu_us.len() as u32 >= self.target {
            self.summarize();
            self.summarized = true;
        }
    }

    fn summarize(&self) {
        let cpu = Summary::of(&self.frame_cpu_us);
        let flush = Summary::of(&self.frame_flush_us);
        let fence = Summary::of(&self.frame_fence_us);
        let gpu = Summary::of(&self.frame_gpu_ms);
        info!(
            "rive zero-copy PERF (frames={}, instances={}): frame CPU [us] {} | \
             rive flush [us] {} | fence wait [us] {} | rive GPU [ms] {}",
            cpu.n,
            self.instances,
            cpu.fmt_us(),
            flush.fmt_us(),
            fence.fmt_us(),
            if gpu.n > 0 {
                gpu.fmt_ms()
            } else {
                "unavailable".to_string()
            },
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
    /// Monotonic frame counter for rive's resource-recycling watermark.
    frame: u64,
    /// Set once we have logged the active PLS mode.
    logged_mode: bool,
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
            frame: 0,
            logged_mode: false,
            clockwise_applied: false,
            perf: PerfStats::default(),
        }))
    }
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
    /// `dt * speed`, sanitized non-negative + finite.
    step: f32,
}

/// Render-world resource holding this frame's extracted rive instances. Replaced
/// wholesale each frame by [`extract_rive_instances`]; read by [`RiveFillNode`].
#[derive(Resource, Default)]
struct ExtractedRives(Vec<ExtractedRive>);

// ===========================================================================
// Main-world system: allocate the display Image (the frozen seam).
// ===========================================================================

/// Allocates the display [`Image`] for each M1b entity whose `.riv` has loaded
/// and whose [`RiveTarget`] has no image yet, then writes the handle back. The
/// image is GPU-only (`data: None`), `Rgba8UnormSrgb`, with the usages the
/// render-graph copy + the sprite sample need.
fn allocate_display_images(
    mut query: Query<(&RiveAnimation, &mut RiveTarget)>,
    files: Res<Assets<RiveFile>>,
    mut images: ResMut<Assets<Image>>,
) {
    for (anim, mut target) in &mut query {
        if target.image != Handle::default() {
            continue;
        }
        if files.get(&anim.handle).is_none() {
            continue; // not loaded yet
        }
        target.image = images.add(make_display_image(target.width, target.height));
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
) {
    if existing.is_some() {
        return;
    }
    // SAFETY: the guards are held only for the extraction; raw handles are copied
    // out as integers and remain valid while Bevy's RenderDevice lives (which
    // outlives the render world). We never store the guards.
    let handles = unsafe {
        let Some(dev_g) = device.wgpu_device().as_hal::<Vk>() else {
            error!("rive zero-copy: wgpu device is not Vulkan (set WGPU_BACKEND=vulkan)");
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
        let clockwise = std::env::var_os("RIVE_CLOCKWISE").is_some();
        let perf_enabled = std::env::var_os("RIVE_PERF").is_some();
        let perf_target = std::env::var("RIVE_PERF_FRAMES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(300u32)
            .max(1);
        // M2a: default to the non-blocking record-into-wgpu path; RIVE_BLOCKING=1
        // selects the M1b blocking submit+fence (fallback / A-B baseline).
        let blocking_submit = std::env::var_os("RIVE_BLOCKING").is_some();

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
    commands.insert_resource(handles);
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
fn extract_rive_instances(
    mut out: ResMut<ExtractedRives>,
    query: Extract<Query<(Entity, &RiveAnimation, &RiveTarget)>>,
    files: Extract<Res<Assets<RiveFile>>>,
    time: Extract<Res<Time>>,
) {
    out.0.clear();
    let dt = time.delta_secs();
    for (entity, anim, target) in &query {
        if target.image == Handle::default() {
            continue;
        }
        let Some(file) = files.get(&anim.handle) else {
            continue;
        };
        let step = dt * anim.speed;
        let step = if step.is_finite() { step.max(0.0) } else { 0.0 };
        out.0.push(ExtractedRive {
            entity,
            display: target.image.clone(),
            bytes: file.bytes.clone(),
            width: target.width,
            height: target.height,
            step,
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
        let frame_items: Vec<ExtractedRive> = world.resource::<ExtractedRives>().0.clone();

        gpu.frame = gpu.frame.wrapping_add(1).max(1);
        let frame_no = gpu.frame;
        let queue = handles.queue;
        // M2a: non-blocking record-into-wgpu by default; RIVE_BLOCKING uses the M1b
        // blocking submit+fence path (needs `queue`).
        let blocking = handles.blocking_submit;

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
            logged_mode,
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
        let mut frame_cpu_us = 0.0_f64;
        let mut frame_flush_us = 0.0_f64;
        let mut frame_fence_us = 0.0_f64;
        let mut frame_gpu_ms = 0.0_f64;
        // GPU timing is all-or-nothing per frame: if any submit lacked a timestamp,
        // the frame's GPU total is not reported (avoids undercounting).
        let mut frame_gpu_ok = true;

        for item in &frame_items {
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

            // Advance the state machine, then run rive's frame. Two paths:
            //  * M2a non-blocking (default): RECORD rive's draws into wgpu's OWN open
            //    command buffer (no submit, no fence) — rive's work rides wgpu's
            //    per-frame submit, GPU-ordered before the un-premult pass below.
            //  * M1b blocking (RIVE_BLOCKING=1): out-of-band submit + blocking fence,
            //    kept as a selectable fallback and an A/B baseline on one build.
            inst.state_machine.advance(item.step);

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
                    safe_frame: frame_no.saturating_sub(1),
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
                    // No fence proves GPU completion, so a frame's resources are safe
                    // to recycle only once it has finished; trail by rive's ring size
                    // (frames-in-flight bounded by the vsync surface).
                    safe_frame: frame_no.saturating_sub(RIVE_RING_SIZE),
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
            let device = render_device.wgpu_device();
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("rive_unpremult_bg"),
                layout: &blit.layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&inst.shared_view),
                }],
            });
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
                pass.set_bind_group(0, &bind_group, &[]);
                pass.draw(0..3, 0..1);
            }
        }

        // M2a: record this frame's aggregate perf (summed over instances), only
        // when work actually ran — so warm-up / `seen` count real frames, not the
        // empty early node calls before the asset + display image are ready.
        if frame_instances > 0 {
            perf.record_frame(
                frame_instances,
                frame_cpu_us,
                frame_flush_us,
                frame_fence_us,
                frame_gpu_ok.then_some(frame_gpu_ms),
            );
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
                } else {
                    "non-blocking (M2a record-into-wgpu)"
                },
                if ctx.last_gpu_ms().is_some() {
                    "available"
                } else {
                    "unavailable"
                },
            );
            // M2a sync guard: the non-blocking watermark (safe_frame = current -
            // RIVE_RING_SIZE) is sound only while frames-in-flight ≤ RIVE_RING_SIZE.
            // Warn (once) if the live window config could exceed that — a non-vsync
            // present mode, or a frame latency past the ring — since rive would then
            // recycle a pooled buffer the GPU is still reading (silent corruption).
            // RIVE_BLOCKING=1 is the safe fallback. See RIVE_RING_SIZE.
            if !blocking {
                if let Some(windows) = world.get_resource::<ExtractedWindows>() {
                    for w in windows.windows.values() {
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
                        if in_flight > RIVE_RING_SIZE {
                            warn!(
                                "rive zero-copy: non-blocking sync assumes ≤ {RIVE_RING_SIZE} frames in \
                                 flight, but window present_mode={:?} / desired_maximum_frame_latency={:?} \
                                 may exceed it — rive can recycle a pooled buffer the GPU is still reading \
                                 (silent corruption). Use PresentMode::Fifo with latency ≤ {}, or RIVE_BLOCKING=1.",
                                w.present_mode,
                                w.desired_maximum_frame_latency,
                                RIVE_RING_SIZE - 1,
                            );
                        }
                    }
                }
            }
            *logged_mode = true;
        }

        // Drop instances whose entity is no longer extracted this frame.
        let live: std::collections::HashSet<Entity> =
            frame_items.iter().map(|x| x.entity).collect();
        instances.retain(|e, _| live.contains(e));

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
    let file = ctx.load_file(&item.bytes)?;
    let artboard = file.default_artboard()?;
    let state_machine = artboard.default_state_machine()?;

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
        display: item.display.clone(),
    })
}
