//! Raw FFI bindings to the native Rive Renderer via the `rive_shim` C ABI.
//!
//! This crate hand-declares the `extern "C"` surface implemented by
//! `shim/rive_shim.cpp` (see [`rive_shim.h`] for the contract). It builds the
//! rive-runtime PLS Vulkan static libraries and the shim in [`build.rs`], then
//! links them. Everything here is unsafe and untyped; the safe RAII wrapper
//! lives in the `rive-renderer` crate.
//!
//! [`rive_shim.h`]: ../../../crates/rive-renderer-sys/shim/rive_shim.h
//!
//! # Safety
//!
//! All handles are opaque pointers owned by the shim. Each `*_create*`/`*_load`
//! function returns a pointer that must be released with its matching
//! `*_destroy` function exactly once. Handles are not thread-safe and a single
//! context allows only one in-flight frame (`begin` → `draw` → `flush`).
#![expect(
    missing_debug_implementations,
    reason = "opaque FFI handle types are only ever used behind raw pointers"
)]

use std::os::raw::c_char;

/// Status returned by fallible shim verbs; [`RIVE_OK`] (0) means success.
pub type RiveStatus = i32;

/// Success value for [`RiveStatus`].
pub const RIVE_OK: RiveStatus = 0;

/// rive's active PLS interlock mode (`gpu::InterlockMode` ordinal), as returned
/// by [`rive_render_context_pls_mode`]. `-1` means a null handle / not in a frame.
pub type RivePlsMode = i32;

/// `gpu::InterlockMode::rasterOrdering` — the clean raster-order PLS path.
pub const RIVE_PLS_RASTER_ORDERING: RivePlsMode = 0;
/// `gpu::InterlockMode::atomics` — the atomic fallback (no interlock).
pub const RIVE_PLS_ATOMICS: RivePlsMode = 1;
/// `gpu::InterlockMode::clockwise`.
pub const RIVE_PLS_CLOCKWISE: RivePlsMode = 2;
/// `gpu::InterlockMode::clockwiseAtomic`.
pub const RIVE_PLS_CLOCKWISE_ATOMIC: RivePlsMode = 3;
/// `gpu::InterlockMode::msaa`.
pub const RIVE_PLS_MSAA: RivePlsMode = 4;

/// C-stable mirror of `rive::gpu::VulkanFeatures` (M1b external Vulkan tier).
///
/// The caller fills this from the features wgpu actually enabled on the shared
/// `VkDevice`; the shim copies it field-by-field into rive's struct. Bools are
/// `i32` (0 == false, nonzero == true) for a stable ABI.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RiveVulkanFeatures {
    /// Vulkan API version (e.g. `VK_API_VERSION_1_1`).
    pub api_version: u32,
    /// `VkPhysicalDeviceFeatures::independentBlend`.
    pub independent_blend: i32,
    /// `VkPhysicalDeviceFeatures::fillModeNonSolid`.
    pub fill_mode_non_solid: i32,
    /// `VkPhysicalDeviceFeatures::fragmentStoresAndAtomics` (required by rive).
    pub fragment_stores_and_atomics: i32,
    /// `VkPhysicalDeviceFeatures::shaderClipDistance`.
    pub shader_clip_distance: i32,
    /// `VK_EXT_rasterization_order_attachment_access`.
    pub rasterization_order_color_attachment_access: i32,
    /// `VK_EXT_fragment_shader_interlock` (pixel interlock).
    pub fragment_shader_pixel_interlock: i32,
    /// `VK_KHR_portability_subset` (nonconformant driver, e.g. MoltenVK).
    pub vk_khr_portability_subset: i32,
    /// BC texture compression.
    pub texture_compression_bc: i32,
    /// ASTC LDR texture compression.
    pub texture_compression_astc_ldr: i32,
    /// ETC2 texture compression.
    pub texture_compression_etc2: i32,
}

/// Opaque self-managed Vulkan render context (owns its `VkInstance`/`VkDevice`).
#[repr(C)]
pub struct RiveRenderContext {
    _opaque: [u8; 0],
}

/// Opaque offscreen render target (rive render target + headless synchronizer).
#[repr(C)]
pub struct RiveRenderTarget {
    _opaque: [u8; 0],
}

/// Opaque imported `.riv` file.
#[repr(C)]
pub struct RiveFile {
    _opaque: [u8; 0],
}

/// Opaque artboard instance.
#[repr(C)]
pub struct RiveArtboard {
    _opaque: [u8; 0],
}

/// Opaque state machine / scene instance.
#[repr(C)]
pub struct RiveStateMachine {
    _opaque: [u8; 0],
}

/// Opaque view-model instance (the artboard's root VM, a nested VM, or a list
/// item). A **borrowed** handle: it aliases an instance owned by rive's caches
/// under the root view model, so it is valid only while the owning artboard lives
/// (and, for list items, while the addressed list is unmodified). Never freed.
#[repr(C)]
pub struct RiveViewModelInstance {
    _opaque: [u8; 0],
}

extern "C" {
    /// Returns a static description of the most recent shim failure.
    pub fn rive_last_error() -> *const c_char;

    pub fn rive_render_context_create_vulkan_self() -> *mut RiveRenderContext;
    pub fn rive_render_context_destroy(ctx: *mut RiveRenderContext);

    pub fn rive_render_target_create_offscreen(
        ctx: *mut RiveRenderContext,
        width: u32,
        height: u32,
    ) -> *mut RiveRenderTarget;
    pub fn rive_render_target_destroy(target: *mut RiveRenderTarget);
    pub fn rive_render_target_width(target: *const RiveRenderTarget) -> u32;
    pub fn rive_render_target_height(target: *const RiveRenderTarget) -> u32;
    pub fn rive_render_target_pixel_buffer_size(target: *const RiveRenderTarget) -> usize;

    pub fn rive_file_load(
        ctx: *mut RiveRenderContext,
        bytes: *const u8,
        len: usize,
    ) -> *mut RiveFile;
    pub fn rive_file_destroy(file: *mut RiveFile);

    pub fn rive_file_artboard_default(file: *mut RiveFile) -> *mut RiveArtboard;
    pub fn rive_artboard_destroy(artboard: *mut RiveArtboard);

    pub fn rive_artboard_state_machine_default(
        artboard: *mut RiveArtboard,
    ) -> *mut RiveStateMachine;
    pub fn rive_state_machine_destroy(sm: *mut RiveStateMachine);
    pub fn rive_state_machine_advance(sm: *mut RiveStateMachine, dt_seconds: f32);

    // Pointer input → state-machine Listeners. `x,y` are in target-pixel space
    // (0..w, 0..h, top-left origin); `w,h` are the render-target pixel size those
    // coords are relative to. The shim inverts the SAME Fit::contain/center
    // alignment used to draw, then forwards to the rive `Scene`. Each returns the
    // `rive::HitResult` as a byte (0 none / 1 hit / 2 hitOpaque; 0 on bad args).
    pub fn rive_state_machine_pointer_move(
        sm: *mut RiveStateMachine,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    ) -> u8;
    pub fn rive_state_machine_pointer_down(
        sm: *mut RiveStateMachine,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    ) -> u8;
    pub fn rive_state_machine_pointer_up(
        sm: *mut RiveStateMachine,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    ) -> u8;
    pub fn rive_state_machine_pointer_exit(
        sm: *mut RiveStateMachine,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    ) -> u8;

    // ===== View-model data binding ==========================================
    // Get/set named view-model properties on an artboard's bound default
    // instance (see shim/rive_shim_viewmodel.cpp). `path` is a UTF-8 C string;
    // verbs return RiveStatus (0 ok; nonzero + rive_last_error). Slice 1:
    // number/bool/trigger + schema introspection.
    pub fn rive_artboard_vm_set_number(
        artboard: *mut RiveArtboard,
        path: *const c_char,
        value: f32,
    ) -> RiveStatus;
    pub fn rive_artboard_vm_get_number(
        artboard: *mut RiveArtboard,
        path: *const c_char,
        out: *mut f32,
    ) -> RiveStatus;
    pub fn rive_artboard_vm_set_bool(
        artboard: *mut RiveArtboard,
        path: *const c_char,
        value: u8,
    ) -> RiveStatus;
    pub fn rive_artboard_vm_get_bool(
        artboard: *mut RiveArtboard,
        path: *const c_char,
        out: *mut u8,
    ) -> RiveStatus;
    pub fn rive_artboard_vm_fire_trigger(
        artboard: *mut RiveArtboard,
        path: *const c_char,
    ) -> RiveStatus;
    // Slice 2: color (ARGB u32), string, enum. Strings/enum-names use the
    // two-call buffer protocol (buf=null, cap=0 to size; bytes not NUL-terminated).
    pub fn rive_artboard_vm_set_color(
        artboard: *mut RiveArtboard,
        path: *const c_char,
        argb: u32,
    ) -> RiveStatus;
    pub fn rive_artboard_vm_get_color(
        artboard: *mut RiveArtboard,
        path: *const c_char,
        out: *mut u32,
    ) -> RiveStatus;
    pub fn rive_artboard_vm_set_string(
        artboard: *mut RiveArtboard,
        path: *const c_char,
        value: *const c_char,
    ) -> RiveStatus;
    pub fn rive_artboard_vm_get_string(
        artboard: *mut RiveArtboard,
        path: *const c_char,
        buf: *mut c_char,
        cap: usize,
        out_len: *mut usize,
    ) -> RiveStatus;
    pub fn rive_artboard_vm_set_enum_index(
        artboard: *mut RiveArtboard,
        path: *const c_char,
        index: u32,
    ) -> RiveStatus;
    pub fn rive_artboard_vm_get_enum_index(
        artboard: *mut RiveArtboard,
        path: *const c_char,
        out: *mut u32,
    ) -> RiveStatus;
    pub fn rive_artboard_vm_set_enum_name(
        artboard: *mut RiveArtboard,
        path: *const c_char,
        name: *const c_char,
    ) -> RiveStatus;
    pub fn rive_artboard_vm_enum_value_count(
        artboard: *mut RiveArtboard,
        path: *const c_char,
        out: *mut u32,
    ) -> RiveStatus;
    pub fn rive_artboard_vm_enum_value_at(
        artboard: *mut RiveArtboard,
        path: *const c_char,
        index: u32,
        buf: *mut c_char,
        cap: usize,
        out_len: *mut usize,
    ) -> RiveStatus;
    pub fn rive_artboard_vm_property_count(artboard: *mut RiveArtboard) -> u32;
    pub fn rive_artboard_vm_property_at(
        artboard: *mut RiveArtboard,
        index: u32,
        name_buf: *mut c_char,
        cap: usize,
        out_len: *mut usize,
        out_type: *mut i32,
    ) -> RiveStatus;
    // Handle API (nested VMs + lists). Navigation returns a borrowed
    // `*mut RiveViewModelInstance` (null on miss/null-input); reads + introspection
    // mirror the artboard-rooted verbs. `out_type` ordinals add list=5,
    // viewModel=8, assetImage=11, artboard=12 to the scalar set.
    pub fn rive_artboard_vm_root(artboard: *mut RiveArtboard) -> *mut RiveViewModelInstance;
    pub fn rive_vmi_property_view_model(
        vmi: *mut RiveViewModelInstance,
        path: *const c_char,
    ) -> *mut RiveViewModelInstance;
    pub fn rive_vmi_list_size(
        vmi: *mut RiveViewModelInstance,
        path: *const c_char,
        out: *mut u32,
    ) -> RiveStatus;
    pub fn rive_vmi_list_instance_at(
        vmi: *mut RiveViewModelInstance,
        path: *const c_char,
        index: u32,
    ) -> *mut RiveViewModelInstance;
    pub fn rive_vmi_property_count(vmi: *mut RiveViewModelInstance) -> u32;
    pub fn rive_vmi_property_at(
        vmi: *mut RiveViewModelInstance,
        index: u32,
        name_buf: *mut c_char,
        cap: usize,
        out_len: *mut usize,
        out_type: *mut i32,
    ) -> RiveStatus;
    pub fn rive_vmi_get_number(
        vmi: *mut RiveViewModelInstance,
        path: *const c_char,
        out: *mut f32,
    ) -> RiveStatus;
    pub fn rive_vmi_get_bool(
        vmi: *mut RiveViewModelInstance,
        path: *const c_char,
        out: *mut u8,
    ) -> RiveStatus;
    pub fn rive_vmi_get_color(
        vmi: *mut RiveViewModelInstance,
        path: *const c_char,
        out: *mut u32,
    ) -> RiveStatus;
    pub fn rive_vmi_get_string(
        vmi: *mut RiveViewModelInstance,
        path: *const c_char,
        buf: *mut c_char,
        cap: usize,
        out_len: *mut usize,
    ) -> RiveStatus;
    pub fn rive_vmi_get_enum_index(
        vmi: *mut RiveViewModelInstance,
        path: *const c_char,
        out: *mut u32,
    ) -> RiveStatus;

    pub fn rive_frame_begin(
        ctx: *mut RiveRenderContext,
        target: *mut RiveRenderTarget,
        r: f32,
        g: f32,
        b: f32,
        a: f32,
    ) -> RiveStatus;
    pub fn rive_artboard_draw(
        artboard: *mut RiveArtboard,
        ctx: *mut RiveRenderContext,
    ) -> RiveStatus;
    /// Like [`rive_artboard_draw`] but fits + clips the artboard into the sub-rect
    /// `(x,y,w,h)` of the bound target (an atlas tile, target pixels). The clip uses
    /// rive's cheap axis-aligned clipRect shader path (no mask draw). Call between a
    /// begin and a record/flush.
    pub fn rive_artboard_draw_viewport(
        artboard: *mut RiveArtboard,
        ctx: *mut RiveRenderContext,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    ) -> RiveStatus;
    pub fn rive_frame_flush(ctx: *mut RiveRenderContext) -> RiveStatus;

    pub fn rive_render_target_read_pixels(
        target: *mut RiveRenderTarget,
        out_rgba: *mut u8,
        out_len: usize,
    ) -> RiveStatus;

    // ---- M1b: external (wgpu-shared) Vulkan tier --------------------------
    // Vulkan handles cross as `u64` (the ash/wgpu-hal raw handle value);
    // 64-bit hosts only. See `shim/rive_shim.h` for the full contract.

    /// Creates a rive `RenderContext` on a wgpu-owned Vulkan device (borrowed,
    /// never destroyed by the shim). `get_instance_proc_addr` is a
    /// `PFN_vkGetInstanceProcAddr` value. Returns null on failure.
    pub fn rive_render_context_create_vulkan_external(
        instance: u64,
        physical_device: u64,
        device: u64,
        get_instance_proc_addr: *mut std::os::raw::c_void,
        features: *const RiveVulkanFeatures,
        force_atomic: i32,
    ) -> *mut RiveRenderContext;

    /// Sets the graphics queue-family index the shim allocates its per-frame
    /// command pool on. Call once after creating an external context.
    pub fn rive_render_context_set_queue_family(
        ctx: *mut RiveRenderContext,
        queue_family_index: u32,
    );

    /// M2.0 perf lever: enable/disable rive's per-frame `clockwiseFillOverride`
    /// (nonzero == on). Honored by [`rive_frame_begin_external`].
    pub fn rive_render_context_set_clockwise(ctx: *mut RiveRenderContext, enabled: i32);

    /// M2.0: GPU execution time (milliseconds) of the last external frame's rive
    /// command buffer (Vulkan timestamps), or `-1.0` if GPU timing is unavailable.
    pub fn rive_render_context_last_gpu_ms(ctx: *const RiveRenderContext) -> f64;

    /// M2a: CPU sub-span timings (microseconds) of the last external frame —
    /// rive's `flush()` and the blocking `vkWaitForFences` — for the fence-vs-flush
    /// perf split. `-1.0` if no external frame has run yet.
    pub fn rive_render_context_last_flush_us(ctx: *const RiveRenderContext) -> f64;
    pub fn rive_render_context_last_fence_wait_us(ctx: *const RiveRenderContext) -> f64;

    /// 1 if the shared device gives rive the clean raster-order PLS path, 0 if
    /// not (atomic/msaa fallback), -1 on a null handle.
    pub fn rive_render_context_supports_raster_ordering(ctx: *const RiveRenderContext) -> i32;

    /// The active interlock mode (valid only between begin and submit).
    pub fn rive_render_context_pls_mode(ctx: *const RiveRenderContext) -> RivePlsMode;

    /// Wraps a wgpu-allocated `VkImage` as a zero-copy rive render target. Pass
    /// `vk_image_view == 0` to have the shim create a matching view. Returns
    /// null on failure.
    pub fn rive_render_target_wrap_vk_image(
        ctx: *mut RiveRenderContext,
        vk_image: u64,
        vk_image_view: u64,
        width: u32,
        height: u32,
        vk_format: u32,
        vk_usage_flags: u32,
    ) -> *mut RiveRenderTarget;

    /// Rebinds the wgpu `VkImage`/view on an existing external target (e.g.
    /// after a reprepare). Pass `vk_image_view == 0` to keep the current view.
    pub fn rive_render_target_set_vk_image(
        target: *mut RiveRenderTarget,
        vk_image: u64,
        vk_image_view: u64,
    );

    /// Begins a frame against a wrapped external target. The caller supplies the
    /// frame-number watermark (`current_frame_number` must be nonzero).
    pub fn rive_frame_begin_external(
        ctx: *mut RiveRenderContext,
        target: *mut RiveRenderTarget,
        r: f32,
        g: f32,
        b: f32,
        a: f32,
        current_frame_number: u64,
        safe_frame_number: u64,
    ) -> RiveStatus;

    /// Records rive's draws + the post-flush `COLOR -> SHADER_READ_ONLY` barrier
    /// into a shim-owned command buffer, submits it out-of-band to `queue` with a
    /// shim-internal fence, then **blocks** on that fence. On return the shared
    /// image is fully rendered and in `SHADER_READ_ONLY_OPTIMAL` (M1b is
    /// correctness-first; splitting submit/wait is M2).
    pub fn rive_frame_submit_external(
        ctx: *mut RiveRenderContext,
        target: *mut RiveRenderTarget,
        queue: u64,
    ) -> RiveStatus;

    /// M2a NON-BLOCKING path: records rive's draws + the `COLOR -> SHADER_READ_ONLY`
    /// barrier into `cmd_buffer` (wgpu's own open primary `VkCommandBuffer`, as a u64
    /// handle) and returns WITHOUT submit/fence. rive's work rides wgpu's per-frame
    /// submit, GPU-ordered before the wgpu pass that samples the image — no CPU stall.
    pub fn rive_frame_record_external(
        ctx: *mut RiveRenderContext,
        target: *mut RiveRenderTarget,
        cmd_buffer: u64,
    ) -> RiveStatus;

    /// The `VkImage` the external target currently points at (0 if not external).
    pub fn rive_render_target_vk_image(target: *const RiveRenderTarget) -> u64;
    /// The `VkImageView` the external target currently points at (0 if none).
    pub fn rive_render_target_vk_image_view(target: *const RiveRenderTarget) -> u64;

    // ---- Backend-tagged d3d12 / metal siblings (declared; stubbed in M1b) --

    /// d3d12 external context (design-only; returns null in M1b).
    pub fn rive_render_context_create_d3d12_external(
        d3d12_device: *mut std::os::raw::c_void,
        d3d12_command_queue: *mut std::os::raw::c_void,
        force_atomic: i32,
    ) -> *mut RiveRenderContext;
    /// d3d12 external target (design-only; returns null in M1b).
    pub fn rive_render_target_wrap_d3d12_resource(
        ctx: *mut RiveRenderContext,
        d3d12_resource: *mut std::os::raw::c_void,
        width: u32,
        height: u32,
        dxgi_format: u32,
    ) -> *mut RiveRenderTarget;
    /// d3d12 external submit (design-only; returns nonzero in M1b).
    pub fn rive_frame_submit_external_d3d12(
        ctx: *mut RiveRenderContext,
        target: *mut RiveRenderTarget,
        d3d12_command_queue: *mut std::os::raw::c_void,
        d3d12_fence: *mut std::os::raw::c_void,
        fence_value: u64,
    ) -> RiveStatus;
    /// metal external context (design-only; returns null in M1b).
    pub fn rive_render_context_create_metal_external(
        mtl_device: *mut std::os::raw::c_void,
        mtl_command_queue: *mut std::os::raw::c_void,
    ) -> *mut RiveRenderContext;
    /// metal external target (design-only; returns null in M1b).
    pub fn rive_render_target_wrap_metal_texture(
        ctx: *mut RiveRenderContext,
        mtl_texture: *mut std::os::raw::c_void,
        width: u32,
        height: u32,
        mtl_pixel_format: u32,
    ) -> *mut RiveRenderTarget;
    /// metal external submit (design-only; returns nonzero in M1b).
    pub fn rive_frame_submit_external_metal(
        ctx: *mut RiveRenderContext,
        target: *mut RiveRenderTarget,
        mtl_command_buffer: *mut std::os::raw::c_void,
    ) -> RiveStatus;
}
