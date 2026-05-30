//! Safe RAII wrapper over the native Rive Renderer (offscreen Vulkan, M0/M1a).
//!
//! This crate wraps the raw [`rive_renderer_sys`] FFI in `Result`-based,
//! drop-safe handles. Milestone 0 renders a `.riv` file's default state machine
//! to an offscreen image with rive's own (self-managed) Vulkan device and reads
//! the pixels back — no wgpu and no Bevy.
//!
//! # Example
//!
//! ```no_run
//! use rive_renderer::Context;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let ctx = Context::new()?;
//! let target = ctx.offscreen_target(512, 512)?;
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
//! # Ownership, lifetimes & threading
//!
//! Unlike a borrow-checked-against-`&Context` design, every handle here is
//! **owned and `'static`**: it keeps the native Vulkan context alive by holding a
//! shared (`Rc`) reference to it. This lets handles be stored in long-lived
//! containers (e.g. a Bevy `NonSend` resource) without naming a lifetime, which
//! M1a's ECS bridge needs. Concretely:
//!
//! * Every handle ([`RenderTarget`], [`File`], [`Artboard`], [`StateMachine`])
//!   holds an `Rc` to the [`Context`]'s inner state, so the `VkDevice` is
//!   destroyed only after the **last** handle drops — regardless of drop order.
//! * A [`StateMachine`] additionally keeps its [`Artboard`] alive (the native
//!   `rive::Scene` holds a non-owning pointer back to the artboard instance, so
//!   the scene must be destroyed first). A manual `Drop` body always runs before
//!   the handle's fields drop, so each native `*_destroy` precedes the `Rc`
//!   decrement it guards — the required destruction order holds by construction.
//!
//! Because the handles hold `Rc` (and raw pointers), they are **`!Send + !Sync`**
//! and must be used from a single thread (in Bevy: a `NonSend` resource on the
//! main thread). The native renderer is not internally synchronized.
//!
//! # Color contract
//!
//! [`RenderTarget::read_pixels`] returns top-down `RGBA8`, sRGB-encoded, with
//! **premultiplied** alpha (the native renderer's output). Use
//! [`unpremultiply_rgba8`] before handing the bytes to a tool that expects
//! straight alpha, or clear to an opaque color (then premultiplied == straight).

use std::ffi::CStr;
use std::rc::Rc;

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
    /// A handle from a different [`Context`] was mixed into an operation (e.g. a
    /// [`RenderTarget`] or [`Artboard`] built on another context). Doing so would
    /// drive one Vulkan device's objects through another's — undefined behavior —
    /// so it is rejected here rather than executed.
    #[error("handle belongs to a different Context")]
    ContextMismatch,
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

/// Owns the native render context (and its `VkInstance`/`VkDevice`). Shared via
/// `Rc` so every handle can keep it alive; destroyed on the last drop.
///
/// Private: the public surface is [`Context`] and the handle types, all of which
/// hold an `Rc<ContextInner>`. A manual `Drop` runs before the (none) fields, so
/// the native context is torn down here only once the refcount reaches zero —
/// i.e. after every [`RenderTarget`]/[`File`]/[`Artboard`]/[`StateMachine`] has
/// already destroyed its own native object.
struct ContextInner {
    ptr: *mut sys::RiveRenderContext,
}

impl Drop for ContextInner {
    fn drop(&mut self) {
        // SAFETY: `ptr` was created by the shim and is destroyed exactly once,
        // when the last `Rc<ContextInner>` drops. All dependent native objects
        // (targets/files/artboards/scenes) have already been destroyed because
        // their handles each held an `Rc<ContextInner>` released only after they
        // ran their own `*_destroy`.
        unsafe { sys::rive_render_context_destroy(self.ptr) };
    }
}

/// A self-managed Vulkan device hosting the native Rive render context.
///
/// In M0/M1a the context creates and owns its own `VkInstance`/`VkDevice`. Honors
/// the `RIVE_GPU` (GPU-name filter) and `RIVE_FORCE_ATOMIC` environment variables
/// read by the shim at creation time.
///
/// `!Send + !Sync`: use from one thread (a Bevy `NonSend` resource).
pub struct Context {
    inner: Rc<ContextInner>,
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
        Ok(Self {
            inner: Rc::new(ContextInner { ptr }),
        })
    }

    /// Raw context pointer (valid while `self` — or any handle derived from it —
    /// is alive).
    fn raw(&self) -> *mut sys::RiveRenderContext {
        self.inner.ptr
    }

    /// Creates an offscreen `width`x`height` render target on this context.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TargetCreation`] if the dimensions are zero or the GPU
    /// allocation fails.
    pub fn offscreen_target(&self, width: u32, height: u32) -> Result<RenderTarget> {
        // SAFETY: `self.raw()` is a live context for the duration of the call,
        // and the returned target keeps it alive via its `Rc` clone.
        let ptr = unsafe { sys::rive_render_target_create_offscreen(self.raw(), width, height) };
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
            ctx: Rc::clone(&self.inner),
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
    pub fn load_file(&self, bytes: &[u8]) -> Result<File> {
        // SAFETY: `bytes` is a valid slice borrowed only for this call; the
        // returned file keeps the context alive via its `Rc` clone.
        let ptr = unsafe { sys::rive_file_load(self.raw(), bytes.as_ptr(), bytes.len()) };
        if ptr.is_null() {
            return Err(Error::FileLoad(last_error()));
        }
        Ok(File {
            ptr,
            _ctx: Rc::clone(&self.inner),
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
        target: &'a RenderTarget,
        clear_rgba: [f32; 4],
    ) -> Result<Frame<'a>> {
        // The Rc graph proves *some* context is alive, but not that `target`
        // belongs to *this* one. Driving this context's renderer against a target
        // bound to another context's VkDevice is undefined behavior, so reject it.
        if !Rc::ptr_eq(&self.inner, &target.ctx) {
            return Err(Error::ContextMismatch);
        }
        let [r, g, b, a] = clear_rgba;
        // SAFETY: context and target are live for `'a`.
        let status = unsafe { sys::rive_frame_begin(self.raw(), target.ptr, r, g, b, a) };
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
    ///
    /// The pointer is valid only while this `Context` (or a handle derived from
    /// it) is alive, and must not be used to begin/flush a frame while a [`Frame`]
    /// is live or from another thread. Intended for M1b's wgpu interop.
    #[must_use]
    pub fn as_raw(&self) -> *mut sys::RiveRenderContext {
        self.inner.ptr
    }
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context").finish_non_exhaustive()
    }
}

/// An offscreen render target plus its CPU readback buffer.
///
/// Keeps its [`Context`] alive; `!Send + !Sync`.
pub struct RenderTarget {
    ptr: *mut sys::RiveRenderTarget,
    width: u32,
    height: u32,
    /// The owning context. Keeps the device alive *and* identifies which context
    /// this target belongs to (checked in [`Context::begin_frame`]).
    ctx: Rc<ContextInner>,
}

impl RenderTarget {
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
    ///
    /// Valid only while this `RenderTarget` is alive; do not use it with a
    /// different [`Context`] than the one that created it. Intended for M1b.
    #[must_use]
    pub fn as_raw(&self) -> *mut sys::RiveRenderTarget {
        self.ptr
    }
}

impl Drop for RenderTarget {
    fn drop(&mut self) {
        // SAFETY: created by the shim, destroyed once; the `_ctx` `Rc` (dropped
        // after this body) keeps the context alive until after this destroy.
        unsafe { sys::rive_render_target_destroy(self.ptr) };
    }
}

impl std::fmt::Debug for RenderTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RenderTarget")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish()
    }
}

/// An imported `.riv` file.
///
/// Keeps its [`Context`] alive; `!Send + !Sync`.
pub struct File {
    ptr: *mut sys::RiveFile,
    _ctx: Rc<ContextInner>,
}

impl File {
    /// Instantiates the file's default artboard.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoArtboard`] if the file contains no artboards.
    pub fn default_artboard(&self) -> Result<Artboard> {
        // SAFETY: `self.ptr` is a live file handle.
        let ptr = unsafe { sys::rive_file_artboard_default(self.ptr) };
        if ptr.is_null() {
            return Err(Error::NoArtboard(last_error()));
        }
        Ok(Artboard {
            inner: Rc::new(ArtboardInner {
                ptr,
                ctx: Rc::clone(&self._ctx),
            }),
        })
    }
}

impl Drop for File {
    fn drop(&mut self) {
        // SAFETY: created by the shim, destroyed once. Any live Artboard keeps
        // the underlying rive::File data alive via its own native reference, so
        // dropping the File handle before an Artboard is safe.
        unsafe { sys::rive_file_destroy(self.ptr) };
    }
}

impl std::fmt::Debug for File {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("File").finish_non_exhaustive()
    }
}

/// Owns a native artboard instance, shared via `Rc` so a [`StateMachine`] can
/// keep it alive (the native `rive::Scene` points back at it non-owningly).
struct ArtboardInner {
    ptr: *mut sys::RiveArtboard,
    /// The owning context. Keeps the device alive *and* identifies which context
    /// this artboard belongs to (checked in [`Frame::draw`]).
    ctx: Rc<ContextInner>,
}

impl Drop for ArtboardInner {
    fn drop(&mut self) {
        // SAFETY: created by the shim, destroyed exactly once, when the last
        // `Rc<ArtboardInner>` drops — which is after any `StateMachine` built
        // from it has destroyed its scene (it held an `Rc<ArtboardInner>`).
        unsafe { sys::rive_artboard_destroy(self.ptr) };
    }
}

/// An artboard instance, drawable into a [`Frame`].
///
/// A cheap `Rc` handle: instantiating a [`StateMachine`] shares ownership of the
/// same native artboard, so the artboard outlives the scene that points at it.
/// `!Send + !Sync`.
pub struct Artboard {
    inner: Rc<ArtboardInner>,
}

impl Artboard {
    /// Instantiates the artboard's default state machine, falling back to its
    /// default scene (first state machine, else first animation, else static).
    ///
    /// The returned [`StateMachine`] shares ownership of this artboard, so the
    /// artboard stays alive at least as long as the state machine.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoStateMachine`] if nothing is playable.
    pub fn default_state_machine(&self) -> Result<StateMachine> {
        // SAFETY: `self.inner.ptr` is a live artboard handle.
        let ptr = unsafe { sys::rive_artboard_state_machine_default(self.inner.ptr) };
        if ptr.is_null() {
            return Err(Error::NoStateMachine(last_error()));
        }
        Ok(StateMachine {
            ptr,
            _artboard: Rc::clone(&self.inner),
        })
    }
}

impl std::fmt::Debug for Artboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Artboard").finish_non_exhaustive()
    }
}

/// A state machine (or animation/scene) instance driving an [`Artboard`].
///
/// Holds a shared reference to its [`Artboard`] so the native scene never
/// outlives the artboard instance it points at. `!Send + !Sync`.
pub struct StateMachine {
    ptr: *mut sys::RiveStateMachine,
    _artboard: Rc<ArtboardInner>,
}

impl StateMachine {
    /// Advances the state machine by `dt_seconds` and applies it to the artboard.
    pub fn advance(&mut self, dt_seconds: f32) {
        // SAFETY: `self.ptr` is a live state-machine handle.
        unsafe { sys::rive_state_machine_advance(self.ptr, dt_seconds) };
    }
}

impl Drop for StateMachine {
    fn drop(&mut self) {
        // SAFETY: created by the shim, destroyed once. This body runs before the
        // `_artboard` field drops, so the scene is torn down while its backing
        // artboard instance is still alive.
        unsafe { sys::rive_state_machine_destroy(self.ptr) };
    }
}

impl std::fmt::Debug for StateMachine {
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
    _target: &'a RenderTarget,
    finished: bool,
}

impl Frame<'_> {
    /// Draws `artboard` into this frame, fit with contain + center alignment.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Frame`] if no frame is in progress.
    pub fn draw(&self, artboard: &Artboard) -> Result<()> {
        // Reject an artboard built on a different context (cross-device UB).
        if !Rc::ptr_eq(&self.ctx.inner, &artboard.inner.ctx) {
            return Err(Error::ContextMismatch);
        }
        // SAFETY: artboard and context are live; a frame is in progress.
        let status = unsafe { sys::rive_artboard_draw(artboard.inner.ptr, self.ctx.raw()) };
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
        let status = unsafe { sys::rive_frame_flush(self.ctx.raw()) };
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
            unsafe { sys::rive_frame_flush(self.ctx.raw()) };
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

    /// Mixing a handle from one context into another's frame must be rejected
    /// (it would drive one VkDevice's objects through another — UB).
    ///
    /// `#[ignore]`d: it needs **two** real Vulkan devices, which WSL2's
    /// non-conformant Dozen ICD cannot host (creating a second crashes). Run on
    /// real hardware with `cargo test -p rive-renderer -- --ignored`.
    #[test]
    #[ignore = "needs two real Vulkan devices (not WSL2 Dozen)"]
    fn cross_context_handles_are_rejected() {
        let (Ok(ctx_a), Ok(ctx_b)) = (Context::new(), Context::new()) else {
            eprintln!("skipping: needs two Vulkan devices");
            return;
        };
        let target_b = ctx_b.offscreen_target(8, 8).expect("target on ctx_b");
        // begin_frame on ctx_a with ctx_b's target must error, not run.
        assert!(matches!(
            ctx_a.begin_frame(&target_b, [0.0; 4]),
            Err(Error::ContextMismatch)
        ));

        // draw with an artboard from another context must also error. Build a
        // tiny valid frame on ctx_a, then try to draw ctx_b's artboard into it.
        let bytes = std::fs::read("../../assets/coffee_loader.riv");
        let Ok(bytes) = bytes else {
            eprintln!("skipping draw check: asset not found");
            return;
        };
        let artboard_b = ctx_b
            .load_file(&bytes)
            .and_then(|f| f.default_artboard())
            .expect("artboard on ctx_b");
        let target_a = ctx_a.offscreen_target(8, 8).expect("target on ctx_a");
        let frame = ctx_a
            .begin_frame(&target_a, [0.0; 4])
            .expect("frame on ctx_a");
        assert!(matches!(
            frame.draw(&artboard_b),
            Err(Error::ContextMismatch)
        ));
        frame.flush().ok();
    }
}
