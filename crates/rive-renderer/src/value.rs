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

/// How an artboard is scaled/positioned into its draw target. Mirrors rive's
/// `Fit`; the discriminants match the runtime ordinals passed across the FFI, so
/// the variant order MUST NOT change. `Contain` is the default (the historical
/// hardcoded fit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fit {
    /// Stretch to fill the frame, ignoring aspect ratio.
    Fill,
    /// Scale to fit inside the frame, preserving aspect (letterboxed).
    #[default]
    Contain,
    /// Scale to cover the frame, preserving aspect (cropped).
    Cover,
    /// Scale so the content width fits the frame width.
    FitWidth,
    /// Scale so the content height fits the frame height.
    FitHeight,
    /// No fit-scaling — render at scale 1.0. Content grows in *pixels* as it
    /// grows, but the scale (and therefore font size) stays constant.
    None,
    /// Like [`Fit::Contain`] but never scales *up* past 1.0.
    ScaleDown,
    /// Rive layout: resize the artboard to the frame; uses `scale_factor`.
    Layout,
}

/// Where the (fit-scaled) artboard is anchored within the frame. Mirrors rive's
/// named `Alignment` constants; mapped to (x, y) in -1..1 for the FFI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Alignment {
    /// Top-left corner.
    TopLeft,
    /// Top edge, horizontally centered.
    TopCenter,
    /// Top-right corner.
    TopRight,
    /// Left edge, vertically centered.
    CenterLeft,
    /// Centered (the default).
    #[default]
    Center,
    /// Right edge, vertically centered.
    CenterRight,
    /// Bottom-left corner.
    BottomLeft,
    /// Bottom edge, horizontally centered (e.g. a speech bubble that grows upward).
    BottomCenter,
    /// Bottom-right corner.
    BottomRight,
}

impl Alignment {
    /// (x, y) in -1..1 (left/top = -1, center = 0, right/bottom = +1).
    fn xy(self) -> (f32, f32) {
        match self {
            Alignment::TopLeft => (-1.0, -1.0),
            Alignment::TopCenter => (0.0, -1.0),
            Alignment::TopRight => (1.0, -1.0),
            Alignment::CenterLeft => (-1.0, 0.0),
            Alignment::Center => (0.0, 0.0),
            Alignment::CenterRight => (1.0, 0.0),
            Alignment::BottomLeft => (-1.0, 1.0),
            Alignment::BottomCenter => (0.0, 1.0),
            Alignment::BottomRight => (1.0, 1.0),
        }
    }
}

/// A complete fit specification: [`Fit`] + [`Alignment`] + a `scale_factor`
/// (used only by [`Fit::Layout`]). The [`Default`] is `Contain` / `Center` /
/// `1.0` — the historical render transform, so leaving it unset changes nothing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FitAlign {
    /// How to scale the artboard into the target.
    pub fit: Fit,
    /// Where to anchor it within the target.
    pub alignment: Alignment,
    /// Scale multiplier for [`Fit::Layout`]; ignored by other fits.
    pub scale_factor: f32,
}

impl Default for FitAlign {
    fn default() -> Self {
        Self {
            fit: Fit::Contain,
            alignment: Alignment::Center,
            scale_factor: 1.0,
        }
    }
}

impl FitAlign {
    /// The FFI tuple `(fit ordinal, align_x, align_y, scale_factor)`.
    pub(crate) fn to_raw(self) -> (u32, f32, f32, f32) {
        let (ax, ay) = self.alignment.xy();
        (self.fit as u32, ax, ay, self.scale_factor)
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
