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
    pub fn rive_render_target_pixel_buffer_size(
        target: *const RiveRenderTarget,
    ) -> usize;

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
    pub fn rive_frame_flush(ctx: *mut RiveRenderContext) -> RiveStatus;

    pub fn rive_render_target_read_pixels(
        target: *mut RiveRenderTarget,
        out_rgba: *mut u8,
        out_len: usize,
    ) -> RiveStatus;
}
