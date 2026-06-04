//! Small value / parameter types crossing the API: external-frame parameters
//! ([`ExternalFrameSubmit`], [`ExternalFrameRecord`]), the PLS mode report
//! ([`PlsMode`]), and the Vulkan feature mirror ([`VulkanFeatures`]). Grouped
//! here so the render core in `lib.rs` stays focused on `Context`/`Frame`.

use crate::sys;

/// Per-frame submission parameters for [`Context::render_external_frame`] (M1b).
///
/// Bundles rive's frame-number watermark with the wgpu queue the shim submits
/// rive's command buffer to. The fence is shim-internal (the submit blocks), so
/// the caller supplies only the queue.
///
/// [`Context::render_external_frame`]: crate::Context::render_external_frame
#[derive(Debug, Clone, Copy)]
pub struct ExternalFrameSubmit {
    /// Monotonically increasing, **nonzero** frame number for this frame.
    pub current_frame: u64,
    /// Highest frame number the caller has observed the GPU finish (rive recycles
    /// pooled resources up to this watermark). With the blocking submit this is
    /// `current_frame - 1`.
    pub safe_frame: u64,
    /// The wgpu graphics `VkQueue` handle (as a `u64`) to submit rive's command
    /// buffer to, out-of-band.
    pub queue: u64,
}

/// Per-frame parameters for [`Context::record_external_frame`] (M2a non-blocking).
///
/// Like [`ExternalFrameSubmit`] but carries wgpu's open command buffer (rive records
/// into it) instead of a queue: rive's work rides wgpu's submit, so there is no
/// out-of-band submit and no fence.
///
/// [`Context::record_external_frame`]: crate::Context::record_external_frame
#[derive(Debug, Clone, Copy)]
pub struct ExternalFrameRecord {
    /// Monotonically increasing, **nonzero** frame number for this frame.
    pub current_frame: u64,
    /// Highest frame number whose GPU work has actually completed — rive recycles
    /// pooled transient buffers up to this watermark. WITHOUT a blocking fence the
    /// caller must guarantee this names only GPU-finished frames: either an exact
    /// GPU-completion signal (e.g. a timeline semaphore the per-frame submit advances —
    /// the bevy-rive M2b path), or, as a fallback, `current_frame - ring_size` while
    /// frames-in-flight ≤ ring.
    pub safe_frame: u64,
    /// wgpu's open primary `VkCommandBuffer` (as a `u64`) for this frame, obtained via
    /// `CommandEncoder::as_hal_mut(|e| e.raw_handle())`. rive records its draws into it.
    pub command_buffer: u64,
}

/// rive's active PLS interlock mode (see [`Context::pls_mode`]).
///
/// [`Context::pls_mode`]: crate::Context::pls_mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PlsMode {
    /// Clean raster-order PLS (interlock present) — the preferred path.
    RasterOrdering,
    /// Atomic fallback (no interlock).
    Atomics,
    /// Clockwise fill via raster-order hardware.
    Clockwise,
    /// Experimental atomic-without-barriers path.
    ClockwiseAtomic,
    /// MSAA path.
    Msaa,
    /// Unknown / not currently in a frame.
    Unknown,
}

impl PlsMode {
    pub(crate) fn from_raw(v: sys::RivePlsMode) -> Self {
        match v {
            sys::RIVE_PLS_RASTER_ORDERING => PlsMode::RasterOrdering,
            sys::RIVE_PLS_ATOMICS => PlsMode::Atomics,
            sys::RIVE_PLS_CLOCKWISE => PlsMode::Clockwise,
            sys::RIVE_PLS_CLOCKWISE_ATOMIC => PlsMode::ClockwiseAtomic,
            sys::RIVE_PLS_MSAA => PlsMode::Msaa,
            _ => PlsMode::Unknown,
        }
    }
}

/// Safe mirror of `rive::gpu::VulkanFeatures` for [`Context::from_wgpu_vulkan`].
///
/// Fill this from the features wgpu **actually enabled** on the shared device
/// (read `enabled_device_extensions()` off the hal device); a mismatch makes
/// rive emit pipelines the device rejects. `fragment_stores_and_atomics` is
/// required by rive for core operation.
///
/// [`Context::from_wgpu_vulkan`]: crate::Context::from_wgpu_vulkan
#[derive(Debug, Clone, Copy)]
pub struct VulkanFeatures {
    /// Vulkan API version (e.g. `0x0040_1000` for 1.1).
    pub api_version: u32,
    /// `independentBlend`.
    pub independent_blend: bool,
    /// `fillModeNonSolid`.
    pub fill_mode_non_solid: bool,
    /// `fragmentStoresAndAtomics` (required).
    pub fragment_stores_and_atomics: bool,
    /// `shaderClipDistance`.
    pub shader_clip_distance: bool,
    /// `VK_EXT_rasterization_order_attachment_access`.
    pub rasterization_order_color_attachment_access: bool,
    /// `VK_EXT_fragment_shader_interlock`.
    pub fragment_shader_pixel_interlock: bool,
    /// `VK_KHR_portability_subset`.
    pub vk_khr_portability_subset: bool,
    /// BC texture compression.
    pub texture_compression_bc: bool,
    /// ASTC LDR texture compression.
    pub texture_compression_astc_ldr: bool,
    /// ETC2 texture compression.
    pub texture_compression_etc2: bool,
}

impl Default for VulkanFeatures {
    fn default() -> Self {
        Self {
            api_version: 0x0040_1000, // VK_API_VERSION_1_1
            independent_blend: false,
            fill_mode_non_solid: false,
            fragment_stores_and_atomics: false,
            shader_clip_distance: false,
            rasterization_order_color_attachment_access: false,
            fragment_shader_pixel_interlock: false,
            vk_khr_portability_subset: false,
            texture_compression_bc: false,
            texture_compression_astc_ldr: false,
            texture_compression_etc2: false,
        }
    }
}

impl VulkanFeatures {
    pub(crate) fn to_sys(self) -> sys::RiveVulkanFeatures {
        sys::RiveVulkanFeatures {
            api_version: self.api_version,
            independent_blend: i32::from(self.independent_blend),
            fill_mode_non_solid: i32::from(self.fill_mode_non_solid),
            fragment_stores_and_atomics: i32::from(self.fragment_stores_and_atomics),
            shader_clip_distance: i32::from(self.shader_clip_distance),
            rasterization_order_color_attachment_access: i32::from(
                self.rasterization_order_color_attachment_access,
            ),
            fragment_shader_pixel_interlock: i32::from(self.fragment_shader_pixel_interlock),
            vk_khr_portability_subset: i32::from(self.vk_khr_portability_subset),
            texture_compression_bc: i32::from(self.texture_compression_bc),
            texture_compression_astc_ldr: i32::from(self.texture_compression_astc_ldr),
            texture_compression_etc2: i32::from(self.texture_compression_etc2),
        }
    }
}
