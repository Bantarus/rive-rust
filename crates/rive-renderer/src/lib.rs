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
//! the ECS bridge needs. Concretely:
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
//! Because the handles hold raw pointers, they are **`!Send + !Sync`** and must
//! be used **and dropped** from a single thread. The native renderer is not
//! internally synchronized. Both M1a (a main-thread `NonSend` resource) and M1b
//! (a `NonSend` render-world resource, with pipelined rendering disabled for the
//! tier) keep the whole lifecycle — use *and* drop — on one thread, so a
//! non-atomic `Rc` refcount is sound and **no `unsafe Send` is needed**.
//!
//! NOTE for a future cross-thread tier (e.g. M2 re-enabling pipelined rendering):
//! the subtle hazard is the **drop** thread, not the use thread — Bevy decides
//! when and where a ferried render-world `World` tears down, and that is the one
//! thing a "single-threaded use" assertion cannot control. Satisfying `Send` for
//! the *move* is not enough; the refcount decrement must be made sound too —
//! either an atomic `Arc`, or an explicit main-thread teardown of the
//! render-world rive resources. **Do not pair a non-atomic `Rc` with a ferried
//! world.**
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
    /// `true` for an M1b external (wgpu-shared) context; `false` for the M0/M1a
    /// self-managed context. Gates which methods are valid and (shim-side) which
    /// `Drop` semantics run.
    external: bool,
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
            inner: Rc::new(ContextInner {
                ptr,
                external: false,
            }),
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

    // -- M1b: external (wgpu-shared) Vulkan tier ----------------------------

    /// Creates a Rive context on a **wgpu-owned** Vulkan device (M1b zero-copy
    /// tier). The context borrows the device and never destroys it.
    ///
    /// Handles are extracted from wgpu via `wgpu-hal`/`ash` and passed as the
    /// integer value of each Vulkan handle. `features` MUST mirror exactly what
    /// wgpu enabled on `device` (read them off the hal device, do not guess), or
    /// rive may build pipelines the device rejects.
    ///
    /// # Safety
    ///
    /// - `instance`/`physical_device`/`device` must be the live, matching Vulkan
    ///   handles of a wgpu device that outlives this `Context` and every handle
    ///   derived from it; the device must not be destroyed while they are alive.
    /// - `get_instance_proc_addr` must be the device's `PFN_vkGetInstanceProcAddr`.
    /// - All Rive GPU work for this context must run on one thread.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ContextCreation`] if rive could not build a context on
    /// the supplied device.
    pub unsafe fn from_wgpu_vulkan(
        instance: u64,
        physical_device: u64,
        device: u64,
        get_instance_proc_addr: *mut core::ffi::c_void,
        features: &VulkanFeatures,
        force_atomic: bool,
        queue_family_index: u32,
    ) -> Result<Self> {
        let raw = features.to_sys();
        // SAFETY: the caller upholds the handle-validity contract above; the shim
        // copies `raw` by value and never retains the pointer.
        let ptr = unsafe {
            sys::rive_render_context_create_vulkan_external(
                instance,
                physical_device,
                device,
                get_instance_proc_addr,
                &raw,
                i32::from(force_atomic),
            )
        };
        if ptr.is_null() {
            return Err(Error::ContextCreation(last_error()));
        }
        // SAFETY: `ptr` is the just-created external context.
        unsafe { sys::rive_render_context_set_queue_family(ptr, queue_family_index) };
        Ok(Self {
            inner: Rc::new(ContextInner {
                ptr,
                external: true,
            }),
        })
    }

    /// Enables/disables rive's per-frame `clockwiseFillOverride` (M2.0 perf
    /// lever). When on, `render_external_frame` asks rive to prefer its clockwise
    /// PLS path (clockwise if the device supports it, else clockwiseAtomic) over
    /// atomics — the relevant comparison on desktop NVIDIA, which has no
    /// raster-order extension. Off by default; set once after create. Inspect the
    /// resolved mode with [`Self::pls_mode`] after a frame.
    pub fn set_clockwise(&self, enabled: bool) {
        // SAFETY: `self.inner.ptr` is a live context; the shim only flips a bool.
        unsafe { sys::rive_render_context_set_clockwise(self.inner.ptr, i32::from(enabled)) };
    }

    /// GPU execution time (milliseconds) of the most recent
    /// [`Self::render_external_frame`]'s rive command buffer, measured with Vulkan
    /// timestamps. Returns `None` if GPU timing is unavailable (no reliable device
    /// timestamps, or the query setup failed) or no external frame has run yet.
    #[must_use]
    pub fn last_gpu_ms(&self) -> Option<f64> {
        // SAFETY: `self.inner.ptr` is a live context.
        let ms = unsafe { sys::rive_render_context_last_gpu_ms(self.inner.ptr) };
        (ms >= 0.0).then_some(ms)
    }

    /// CPU wall time (microseconds) of rive's `flush()` during the most recent
    /// [`Self::render_external_frame`] — the command-buffer record, isolated from
    /// the blocking fence wait. `None` if no external frame has run yet (M2a Step 0
    /// fence-vs-flush split).
    #[must_use]
    pub fn last_flush_us(&self) -> Option<f64> {
        // SAFETY: `self.inner.ptr` is a live context.
        let us = unsafe { sys::rive_render_context_last_flush_us(self.inner.ptr) };
        (us >= 0.0).then_some(us)
    }

    /// CPU wall time (microseconds) of the blocking `vkWaitForFences` during the
    /// most recent [`Self::render_external_frame`] — the per-frame stall the M2a
    /// non-blocking-sync rework removes. `None` if no external frame has run yet.
    #[must_use]
    pub fn last_fence_wait_us(&self) -> Option<f64> {
        // SAFETY: `self.inner.ptr` is a live context.
        let us = unsafe { sys::rive_render_context_last_fence_wait_us(self.inner.ptr) };
        (us >= 0.0).then_some(us)
    }

    /// `true` if the shared device gives rive its clean raster-order PLS path
    /// (vs the atomic/msaa fallback). Frame-independent; use at init for logging.
    #[must_use]
    pub fn supports_raster_ordering(&self) -> bool {
        // SAFETY: `self.inner.ptr` is a live context.
        unsafe { sys::rive_render_context_supports_raster_ordering(self.inner.ptr) == 1 }
    }

    /// The active interlock mode. Only meaningful **between** the begin and
    /// submit of an external frame; outside one it reflects the previous frame.
    #[must_use]
    pub fn pls_mode(&self) -> PlsMode {
        // SAFETY: `self.inner.ptr` is a live context.
        PlsMode::from_raw(unsafe { sys::rive_render_context_pls_mode(self.inner.ptr) })
    }

    /// Wraps a **wgpu-allocated** `VkImage` as a zero-copy render target (M1b).
    /// Pass `vk_image_view == 0` to have the shim create a matching view.
    ///
    /// # Safety
    ///
    /// `vk_image` (and `vk_image_view`, if nonzero) must be live handles of a
    /// wgpu texture owned by **this** `Context`'s device, of the given
    /// `vk_format`/`vk_usage_flags`, and must outlive the returned target.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TargetCreation`] if this is not an external context or
    /// rive could not wrap the image.
    pub unsafe fn wrap_vk_image(
        &self,
        vk_image: u64,
        vk_image_view: u64,
        width: u32,
        height: u32,
        vk_format: u32,
        vk_usage_flags: u32,
    ) -> Result<RenderTarget> {
        if !self.inner.external {
            return Err(Error::TargetCreation {
                width,
                height,
                detail: "wrap_vk_image requires an external (wgpu-shared) context".into(),
            });
        }
        // SAFETY: the caller upholds the handle-validity contract; the returned
        // target keeps the context alive via its `Rc` clone.
        let ptr = unsafe {
            sys::rive_render_target_wrap_vk_image(
                self.inner.ptr,
                vk_image,
                vk_image_view,
                width,
                height,
                vk_format,
                vk_usage_flags,
            )
        };
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

    /// Drives one M1b frame: begin → draw `artboard` → record + **out-of-band
    /// submit** to `queue` with `fence`. rive records its draws into a
    /// shim-owned command buffer and the shim submits it; rive never submits
    /// itself. Does **not** wait — the caller waits `fence` before sampling the
    /// target image.
    ///
    /// `submit.queue` is the wgpu graphics `VkQueue` to submit rive's command
    /// buffer to, out-of-band. The call **blocks** until rive's GPU work
    /// completes (the shim waits an internal fence), so on success the target
    /// image is ready to sample.
    ///
    /// # Safety
    ///
    /// `submit.queue` must be the wgpu graphics `VkQueue` of this context's
    /// device. The caller is responsible for serializing use of that queue
    /// against wgpu's own submissions.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ContextMismatch`] if `target`/`artboard` belong to
    /// another context, or [`Error::Frame`] if begin/draw/submit fails.
    pub unsafe fn render_external_frame(
        &self,
        target: &RenderTarget,
        artboard: &Artboard,
        clear_rgba: [f32; 4],
        submit: ExternalFrameSubmit,
    ) -> Result<()> {
        if !Rc::ptr_eq(&self.inner, &target.ctx) || !Rc::ptr_eq(&self.inner, &artboard.inner.ctx) {
            return Err(Error::ContextMismatch);
        }
        let [r, g, b, a] = clear_rgba;
        // SAFETY: context and target are live for the call; the caller upholds
        // the queue/fence contract.
        let begin = unsafe {
            sys::rive_frame_begin_external(
                self.inner.ptr,
                target.ptr,
                r,
                g,
                b,
                a,
                submit.current_frame,
                submit.safe_frame,
            )
        };
        if begin != sys::RIVE_OK {
            return Err(Error::Frame(last_error()));
        }
        // A frame is now in progress. Always reach `submit` so the context is not
        // left wedged mid-frame, then surface the first error.
        // SAFETY: a frame is in progress on this live context; artboard is live.
        let draw = unsafe { sys::rive_artboard_draw(artboard.inner.ptr, self.inner.ptr) };
        let draw_err = (draw != sys::RIVE_OK).then(last_error);
        // SAFETY: a frame is in progress; queue per the caller contract.
        let submit_status =
            unsafe { sys::rive_frame_submit_external(self.inner.ptr, target.ptr, submit.queue) };
        if let Some(e) = draw_err {
            return Err(Error::Frame(e));
        }
        if submit_status != sys::RIVE_OK {
            return Err(Error::Frame(last_error()));
        }
        Ok(())
    }

    /// Drives one **non-blocking** M2a frame: begin → draw `artboard` → **record**
    /// rive's draws + the `COLOR -> SHADER_READ_ONLY` barrier into wgpu's open
    /// command buffer (`frame.command_buffer`). Unlike [`Self::render_external_frame`]
    /// this does **not** submit or wait — rive's work rides wgpu's per-frame submit
    /// and is GPU-ordered before the wgpu pass that samples the target image. Returns
    /// immediately (no CPU stall); the blocking fence is gone.
    ///
    /// # Safety
    ///
    /// `frame.command_buffer` must be wgpu's open primary `VkCommandBuffer` for the
    /// current frame, on this context's device, in the recording state, and must not
    /// be ended/submitted until after this returns (wgpu does that at `finish`).
    /// `frame.safe_frame` must trail `frame.current_frame` by at least rive's
    /// resource-ring size — there is no fence proving GPU completion, so the caller
    /// must bound frames-in-flight accordingly.
    ///
    /// # Errors
    ///
    /// [`Error::ContextMismatch`] if `target`/`artboard` belong to another context,
    /// or [`Error::Frame`] if begin/draw/record fails.
    pub unsafe fn record_external_frame(
        &self,
        target: &RenderTarget,
        artboard: &Artboard,
        clear_rgba: [f32; 4],
        frame: ExternalFrameRecord,
    ) -> Result<()> {
        if !Rc::ptr_eq(&self.inner, &target.ctx) || !Rc::ptr_eq(&self.inner, &artboard.inner.ctx) {
            return Err(Error::ContextMismatch);
        }
        let [r, g, b, a] = clear_rgba;
        // SAFETY: context and target are live for the call; the caller upholds the
        // command-buffer and safe_frame contract.
        let begin = unsafe {
            sys::rive_frame_begin_external(
                self.inner.ptr,
                target.ptr,
                r,
                g,
                b,
                a,
                frame.current_frame,
                frame.safe_frame,
            )
        };
        if begin != sys::RIVE_OK {
            return Err(Error::Frame(last_error()));
        }
        // A frame is in progress. Always reach `record` so the context is not left
        // wedged mid-frame, then surface the first error.
        // SAFETY: a frame is in progress on this live context; artboard is live.
        let draw = unsafe { sys::rive_artboard_draw(artboard.inner.ptr, self.inner.ptr) };
        let draw_err = (draw != sys::RIVE_OK).then(last_error);
        // SAFETY: a frame is in progress; command_buffer is wgpu's open buffer per
        // the caller contract.
        let rec = unsafe {
            sys::rive_frame_record_external(self.inner.ptr, target.ptr, frame.command_buffer)
        };
        if let Some(e) = draw_err {
            return Err(Error::Frame(e));
        }
        if rec != sys::RIVE_OK {
            return Err(Error::Frame(last_error()));
        }
        Ok(())
    }

    /// **Spike (M-SCALE batching).** Like [`Self::record_external_frame`] but draws
    /// **every** artboard in `artboards` into a *single* begin→record cycle — one
    /// `beginFrame`, N `draw`s, one `flush` — instead of N separate frames. This
    /// isolates the per-flush fixed overhead (begin/flush/barrier + the per-frame
    /// buffer set) that real batching would remove. All artboards align to `target`
    /// (overlapping), so this measures CPU **record** cost, not final pixels; a
    /// shipping path would need an atlas target with per-artboard viewports.
    ///
    /// # Safety
    ///
    /// Same contract as [`Self::record_external_frame`] (`frame.command_buffer` is
    /// wgpu's open primary buffer on this device, not ended until this returns).
    ///
    /// # Errors
    ///
    /// [`Error::ContextMismatch`] if `target` or any artboard belongs to another
    /// context, or [`Error::Frame`] if begin/draw/record fails.
    pub unsafe fn record_external_frame_batched(
        &self,
        target: &RenderTarget,
        artboards: &[&Artboard],
        clear_rgba: [f32; 4],
        frame: ExternalFrameRecord,
        clip: bool,
    ) -> Result<()> {
        if !Rc::ptr_eq(&self.inner, &target.ctx) {
            return Err(Error::ContextMismatch);
        }
        for ab in artboards {
            if !Rc::ptr_eq(&self.inner, &ab.inner.ctx) {
                return Err(Error::ContextMismatch);
            }
        }
        let [r, g, b, a] = clear_rgba;
        // SAFETY: context and target are live; the caller upholds the cmd-buffer contract.
        let begin = unsafe {
            sys::rive_frame_begin_external(
                self.inner.ptr,
                target.ptr,
                r,
                g,
                b,
                a,
                frame.current_frame,
                frame.safe_frame,
            )
        };
        if begin != sys::RIVE_OK {
            return Err(Error::Frame(last_error()));
        }
        // A frame is in progress. Draw every artboard into it, then ALWAYS reach
        // `record` so the context is not left wedged mid-frame.
        let mut draw_err = None;
        for ab in artboards {
            // `clip=true` routes through draw_viewport with the FULL-target rect: same
            // alignment as `draw`, plus the per-draw clipRect — isolates the clip CPU
            // cost for the Phase-1 A/B (the tiled atlas path uses real per-tile rects).
            let draw = if clip {
                // SAFETY: a frame is in progress on this live context; artboard is live.
                unsafe {
                    sys::rive_artboard_draw_viewport(
                        ab.inner.ptr,
                        self.inner.ptr,
                        0.0,
                        0.0,
                        target.width as f32,
                        target.height as f32,
                    )
                }
            } else {
                // SAFETY: a frame is in progress on this live context; artboard is live.
                unsafe { sys::rive_artboard_draw(ab.inner.ptr, self.inner.ptr) }
            };
            if draw != sys::RIVE_OK {
                draw_err = Some(last_error());
                break;
            }
        }
        // SAFETY: a frame is in progress; command_buffer is wgpu's open buffer.
        let rec = unsafe {
            sys::rive_frame_record_external(self.inner.ptr, target.ptr, frame.command_buffer)
        };
        if let Some(e) = draw_err {
            return Err(Error::Frame(e));
        }
        if rec != sys::RIVE_OK {
            return Err(Error::Frame(last_error()));
        }
        Ok(())
    }
}

/// Per-frame submission parameters for [`Context::render_external_frame`] (M1b).
///
/// Bundles rive's frame-number watermark with the wgpu queue the shim submits
/// rive's command buffer to. The fence is shim-internal (the submit blocks), so
/// the caller supplies only the queue.
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
    fn from_raw(v: sys::RivePlsMode) -> Self {
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
    fn to_sys(self) -> sys::RiveVulkanFeatures {
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

    /// Rebinds the wgpu `VkImage`/view on an external (M1b) target — e.g. after
    /// Bevy re-prepared the `GpuImage` at the same size. Pass `vk_image_view ==
    /// 0` to keep the current view. Resets the tracked layout to undefined.
    ///
    /// # Safety
    ///
    /// `vk_image` (and `vk_image_view`, if nonzero) must be live handles of a
    /// wgpu texture owned by this target's context device.
    pub unsafe fn set_vk_image(&self, vk_image: u64, vk_image_view: u64) {
        // SAFETY: `self.ptr` is a live target; the caller upholds handle validity.
        unsafe { sys::rive_render_target_set_vk_image(self.ptr, vk_image, vk_image_view) };
    }

    /// The `VkImage` this external target currently points at (0 if not external).
    #[must_use]
    pub fn vk_image(&self) -> u64 {
        // SAFETY: `self.ptr` is a live target.
        unsafe { sys::rive_render_target_vk_image(self.ptr) }
    }

    /// The `VkImageView` this external target currently points at (0 if none).
    #[must_use]
    pub fn vk_image_view(&self) -> u64 {
        // SAFETY: `self.ptr` is a live target.
        unsafe { sys::rive_render_target_vk_image_view(self.ptr) }
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

    /// Draws `artboard` into the sub-rect `(x, y, w, h)` of this frame's target — an
    /// atlas tile, in target pixels — fit with contain + center and **clipped** to
    /// the tile so content cannot bleed past it. Use [`Self::draw`] for a
    /// full-target draw. Call multiple `draw_viewport`s between one begin and one
    /// flush to render N artboards into one atlas in a single frame.
    ///
    /// # Errors
    ///
    /// [`Error::ContextMismatch`] if `artboard` belongs to another context, or
    /// [`Error::Frame`] if no frame is in progress or the rect is degenerate.
    pub fn draw_viewport(
        &self,
        artboard: &Artboard,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    ) -> Result<()> {
        if !Rc::ptr_eq(&self.ctx.inner, &artboard.inner.ctx) {
            return Err(Error::ContextMismatch);
        }
        // SAFETY: artboard and context are live; a frame is in progress.
        let status = unsafe {
            sys::rive_artboard_draw_viewport(artboard.inner.ptr, self.ctx.raw(), x, y, w, h)
        };
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
