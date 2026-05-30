//! Safe RAII wrapper over the native Rive Renderer (offscreen Vulkan, M0).
//!
//! This crate wraps the raw [`rive_renderer_sys`] FFI in `Result`-based,
//! drop-safe handles. Milestone 0 renders a `.riv` file's default state machine
//! to an offscreen image with rive's own (self-managed) Vulkan device and reads
//! the pixels back — no wgpu and no Bevy yet.
//!
//! # Example
//!
//! ```no_run
//! use rive_renderer::Context;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let ctx = Context::new()?;
//! let mut target = ctx.offscreen_target(512, 512)?;
//! let file = ctx.load_file(&std::fs::read("assets/coffee_loader.riv")?)?;
//! let artboard = file.default_artboard()?;
//! let mut state_machine = artboard.default_state_machine()?;
//!
//! state_machine.advance(1.0 / 60.0);
//! let frame = ctx.begin_frame(&target, [0.19, 0.19, 0.19, 1.0])?;
//! frame.draw(&artboard)?;
//! frame.flush()?;
//!
//! let mut pixels = vec![0u8; target.pixel_buffer_size()];
//! target.read_pixels(&mut pixels)?;
//! # Ok(())
//! # }
//! ```
//!
//! # Ownership & lifetimes
//!
//! A [`Context`] owns the Vulkan device. [`RenderTarget`], [`File`], and
//! [`Artboard`] borrow the context and cannot outlive it; a [`StateMachine`]
//! borrows its artboard. Every handle frees its native resources on `Drop`.
//!
//! # Color contract
//!
//! [`RenderTarget::read_pixels`] returns top-down `RGBA8`, sRGB-encoded, with
//! **premultiplied** alpha (the native renderer's output). Use
//! [`unpremultiply_rgba8`] before handing the bytes to a tool that expects
//! straight alpha, or clear to an opaque color (then premultiplied == straight).

use std::ffi::CStr;
use std::marker::PhantomData;

use rive_renderer_sys as sys;

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors returned by the safe wrapper.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Creating the Vulkan device / Rive render context failed.
    #[error("failed to create Rive Vulkan context: {0}")]
    ContextCreation(String),
    /// Creating the offscreen render target failed.
    #[error("failed to create {width}x{height} offscreen render target: {detail}")]
    TargetCreation {
        /// Requested width in pixels.
        width: u32,
        /// Requested height in pixels.
        height: u32,
        /// Underlying shim error.
        detail: String,
    },
    /// Importing the `.riv` bytes failed.
    #[error("failed to load .riv file: {0}")]
    FileLoad(String),
    /// The file had no default artboard.
    #[error("file has no default artboard: {0}")]
    NoArtboard(String),
    /// The artboard had no playable state machine, animation, or scene.
    #[error("artboard has no playable state machine: {0}")]
    NoStateMachine(String),
    /// A frame operation (begin / draw / flush) failed.
    #[error("frame operation failed: {0}")]
    Frame(String),
    /// Reading pixels back failed, or the destination buffer was the wrong size.
    #[error("pixel readback failed: {0}")]
    ReadPixels(String),
}

/// Returns the shim's most recent error string (empty if none).
fn last_error() -> String {
    // SAFETY: `rive_last_error` returns a valid, NUL-terminated static C string
    // (or a pointer to an empty string); it is never null.
    unsafe {
        let ptr = sys::rive_last_error();
        if ptr.is_null() {
            String::new()
        } else {
            CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    }
}

/// A self-managed Vulkan device hosting the native Rive render context.
///
/// In M0 the context creates and owns its own `VkInstance`/`VkDevice`. Honors
/// the `RIVE_GPU` (GPU-name filter) and `RIVE_FORCE_ATOMIC` environment
/// variables read by the shim at creation time.
pub struct Context {
    ptr: *mut sys::RiveRenderContext,
}

impl Context {
    /// Creates a headless Vulkan device and a native Rive render context.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ContextCreation`] if no compatible Vulkan device is
    /// available or the loader (`libvulkan.so.1`) is missing.
    pub fn new() -> Result<Self> {
        // SAFETY: the shim returns either a valid owning handle or null.
        let ptr = unsafe { sys::rive_render_context_create_vulkan_self() };
        if ptr.is_null() {
            return Err(Error::ContextCreation(last_error()));
        }
        Ok(Self { ptr })
    }

    /// Creates an offscreen `width`x`height` render target on this context.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TargetCreation`] if the dimensions are zero or the GPU
    /// allocation fails.
    pub fn offscreen_target(&self, width: u32, height: u32) -> Result<RenderTarget<'_>> {
        // SAFETY: `self.ptr` is a live context for the duration of `&self`.
        let ptr = unsafe { sys::rive_render_target_create_offscreen(self.ptr, width, height) };
        if ptr.is_null() {
            return Err(Error::TargetCreation {
                width,
                height,
                detail: last_error(),
            });
        }
        Ok(RenderTarget {
            ptr,
            width,
            height,
            _ctx: PhantomData,
        })
    }

    /// Imports a `.riv` file from memory, using this context as the factory.
    ///
    /// The bytes are only borrowed for the duration of the call.
    ///
    /// # Errors
    ///
    /// Returns [`Error::FileLoad`] if the data is malformed or an unsupported
    /// version.
    pub fn load_file(&self, bytes: &[u8]) -> Result<File<'_>> {
        // SAFETY: `bytes` is a valid slice borrowed only for this call.
        let ptr = unsafe { sys::rive_file_load(self.ptr, bytes.as_ptr(), bytes.len()) };
        if ptr.is_null() {
            return Err(Error::FileLoad(last_error()));
        }
        Ok(File {
            ptr,
            _ctx: PhantomData,
        })
    }

    /// Begins a frame against `target`, clearing to `clear_rgba` (straight,
    /// non-premultiplied, each channel in `[0, 1]`).
    ///
    /// Draw with [`Frame::draw`], then submit with [`Frame::flush`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::Frame`] if the GPU frame could not be started (e.g. a
    /// frame is already in progress).
    pub fn begin_frame<'a>(
        &'a self,
        target: &'a RenderTarget<'a>,
        clear_rgba: [f32; 4],
    ) -> Result<Frame<'a>> {
        let [r, g, b, a] = clear_rgba;
        // SAFETY: context and target are live for `'a`.
        let status = unsafe { sys::rive_frame_begin(self.ptr, target.ptr, r, g, b, a) };
        if status != sys::RIVE_OK {
            return Err(Error::Frame(last_error()));
        }
        Ok(Frame {
            ctx: self,
            _target: target,
            finished: false,
        })
    }

    /// Returns the raw FFI context pointer (escape hatch for future interop).
    #[must_use]
    pub fn as_raw(&self) -> *mut sys::RiveRenderContext {
        self.ptr
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        // SAFETY: `self.ptr` was created by the shim and is destroyed once.
        unsafe { sys::rive_render_context_destroy(self.ptr) };
    }
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context").finish_non_exhaustive()
    }
}

/// An offscreen render target plus its CPU readback buffer.
pub struct RenderTarget<'ctx> {
    ptr: *mut sys::RiveRenderTarget,
    width: u32,
    height: u32,
    _ctx: PhantomData<&'ctx Context>,
}

impl RenderTarget<'_> {
    /// Width in pixels.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Size in bytes of the `RGBA8` readback buffer (`width * height * 4`).
    #[must_use]
    pub fn pixel_buffer_size(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }

    /// Copies the most recently flushed frame into `out` (top-down `RGBA8`,
    /// premultiplied alpha). `out.len()` must equal [`Self::pixel_buffer_size`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::ReadPixels`] if the buffer is the wrong size or no frame
    /// has been flushed yet.
    pub fn read_pixels(&self, out: &mut [u8]) -> Result<()> {
        // SAFETY: `out` is a valid mutable slice; the shim validates the length.
        let status =
            unsafe { sys::rive_render_target_read_pixels(self.ptr, out.as_mut_ptr(), out.len()) };
        if status != sys::RIVE_OK {
            return Err(Error::ReadPixels(last_error()));
        }
        Ok(())
    }

    /// Convenience over [`Self::read_pixels`] returning a freshly allocated buffer.
    ///
    /// # Errors
    ///
    /// See [`Self::read_pixels`].
    pub fn read_pixels_to_vec(&self) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; self.pixel_buffer_size()];
        self.read_pixels(&mut buf)?;
        Ok(buf)
    }

    /// Returns the raw FFI render-target pointer (escape hatch for future interop).
    #[must_use]
    pub fn as_raw(&self) -> *mut sys::RiveRenderTarget {
        self.ptr
    }
}

impl Drop for RenderTarget<'_> {
    fn drop(&mut self) {
        // SAFETY: created by the shim, destroyed once, while the context lives.
        unsafe { sys::rive_render_target_destroy(self.ptr) };
    }
}

impl std::fmt::Debug for RenderTarget<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RenderTarget")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish()
    }
}

/// An imported `.riv` file.
pub struct File<'ctx> {
    ptr: *mut sys::RiveFile,
    _ctx: PhantomData<&'ctx Context>,
}

impl<'ctx> File<'ctx> {
    /// Instantiates the file's default artboard.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoArtboard`] if the file contains no artboards.
    pub fn default_artboard(&self) -> Result<Artboard<'ctx>> {
        // SAFETY: `self.ptr` is a live file handle.
        let ptr = unsafe { sys::rive_file_artboard_default(self.ptr) };
        if ptr.is_null() {
            return Err(Error::NoArtboard(last_error()));
        }
        Ok(Artboard {
            ptr,
            _ctx: PhantomData,
        })
    }
}

impl Drop for File<'_> {
    fn drop(&mut self) {
        // SAFETY: created by the shim, destroyed once. Any live Artboard keeps
        // the underlying rive::File alive via its own reference.
        unsafe { sys::rive_file_destroy(self.ptr) };
    }
}

impl std::fmt::Debug for File<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("File").finish_non_exhaustive()
    }
}

/// An artboard instance, drawable into a [`Frame`].
pub struct Artboard<'ctx> {
    ptr: *mut sys::RiveArtboard,
    _ctx: PhantomData<&'ctx Context>,
}

impl Artboard<'_> {
    /// Instantiates the artboard's default state machine, falling back to its
    /// default scene (first state machine, else first animation, else static).
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoStateMachine`] if nothing is playable.
    pub fn default_state_machine(&self) -> Result<StateMachine<'_>> {
        // SAFETY: `self.ptr` is a live artboard handle.
        let ptr = unsafe { sys::rive_artboard_state_machine_default(self.ptr) };
        if ptr.is_null() {
            return Err(Error::NoStateMachine(last_error()));
        }
        Ok(StateMachine {
            ptr,
            _artboard: PhantomData,
        })
    }
}

impl Drop for Artboard<'_> {
    fn drop(&mut self) {
        // SAFETY: created by the shim, destroyed once, while the context lives.
        unsafe { sys::rive_artboard_destroy(self.ptr) };
    }
}

impl std::fmt::Debug for Artboard<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Artboard").finish_non_exhaustive()
    }
}

/// A state machine (or animation/scene) instance driving an [`Artboard`].
pub struct StateMachine<'ab> {
    ptr: *mut sys::RiveStateMachine,
    _artboard: PhantomData<&'ab Artboard<'ab>>,
}

impl StateMachine<'_> {
    /// Advances the state machine by `dt_seconds` and applies it to the artboard.
    pub fn advance(&mut self, dt_seconds: f32) {
        // SAFETY: `self.ptr` is a live state-machine handle.
        unsafe { sys::rive_state_machine_advance(self.ptr, dt_seconds) };
    }
}

impl Drop for StateMachine<'_> {
    fn drop(&mut self) {
        // SAFETY: created by the shim, destroyed once, while its artboard lives.
        unsafe { sys::rive_state_machine_destroy(self.ptr) };
    }
}

impl std::fmt::Debug for StateMachine<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateMachine").finish_non_exhaustive()
    }
}

/// An in-progress frame. Draw with [`Self::draw`], then submit with
/// [`Self::flush`]. Dropping without flushing submits an empty frame so the
/// context is left ready for the next frame.
#[must_use = "a Frame must be flushed (or it is auto-submitted on drop)"]
pub struct Frame<'a> {
    ctx: &'a Context,
    _target: &'a RenderTarget<'a>,
    finished: bool,
}

impl Frame<'_> {
    /// Draws `artboard` into this frame, fit with contain + center alignment.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Frame`] if no frame is in progress.
    pub fn draw(&self, artboard: &Artboard<'_>) -> Result<()> {
        // SAFETY: artboard and context are live; a frame is in progress.
        let status = unsafe { sys::rive_artboard_draw(artboard.ptr, self.ctx.ptr) };
        if status != sys::RIVE_OK {
            return Err(Error::Frame(last_error()));
        }
        Ok(())
    }

    /// Submits the frame and reads the result back into the target's buffer.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Frame`] if submission or readback fails.
    pub fn flush(mut self) -> Result<()> {
        self.finished = true;
        // SAFETY: a frame is in progress on this live context.
        let status = unsafe { sys::rive_frame_flush(self.ctx.ptr) };
        if status != sys::RIVE_OK {
            return Err(Error::Frame(last_error()));
        }
        Ok(())
    }
}

impl Drop for Frame<'_> {
    fn drop(&mut self) {
        if !self.finished {
            // Submit so the context is not left mid-frame; ignore the result.
            // SAFETY: a frame is in progress on this live context.
            unsafe { sys::rive_frame_flush(self.ctx.ptr) };
        }
    }
}

impl std::fmt::Debug for Frame<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame")
            .field("finished", &self.finished)
            .finish()
    }
}

/// Converts premultiplied `RGBA8` pixels (rive's output) to straight alpha,
/// in place, for tools/viewers that assume non-premultiplied alpha.
///
/// Fully opaque or fully transparent pixels are left unchanged. The slice
/// length must be a multiple of 4; trailing bytes are ignored.
pub fn unpremultiply_rgba8(pixels: &mut [u8]) {
    for px in pixels.chunks_exact_mut(4) {
        let a = px[3];
        if a == 0 || a == 255 {
            continue;
        }
        for channel in &mut px[..3] {
            // round(c * 255 / a), clamped to 255.
            let v = (u32::from(*channel) * 255 + u32::from(a) / 2) / u32::from(a);
            *channel = v.min(255) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpremultiply_leaves_opaque_and_transparent_unchanged() {
        let mut px = [10, 20, 30, 255, 1, 2, 3, 0];
        unpremultiply_rgba8(&mut px);
        assert_eq!(px, [10, 20, 30, 255, 1, 2, 3, 0]);
    }

    #[test]
    fn unpremultiply_scales_partial_alpha() {
        // Premultiplied half-alpha white: rgb = 128, a = 128 -> straight ~255.
        let mut px = [128, 128, 128, 128];
        unpremultiply_rgba8(&mut px);
        assert_eq!(px[3], 128);
        assert!(px[0] >= 254, "expected ~255, got {}", px[0]);
    }
}
