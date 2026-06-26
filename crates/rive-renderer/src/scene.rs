//! The scene graph: an imported [`File`], its [`Artboard`] instances, and the
//! [`StateMachine`] (or animation/scene) that drives one — plus [`HitResult`],
//! the pointer-event outcome. These form one ownership chain (`File` →
//! `Artboard` → `StateMachine`, each keeping the previous alive), so they live
//! together here; the render core (`Context`/`RenderTarget`/`Frame`) stays in
//! `lib.rs`. View-model data binding adds methods to [`Artboard`] in
//! `view_model.rs`.

use std::ffi::CString;
use std::os::raw::c_char;
use std::rc::Rc;

use crate::{last_error, sys, ContextInner, Error, FitAlign, Result};

/// Runs the shim's two-call string protocol (size with a null buffer, then fill)
/// via `call`, returning the bytes as a `String` (empty on a shim error). Used by
/// the selection-introspection accessors. `call(buf, cap, out_len)`.
fn read_name<F>(call: F) -> String
where
    F: Fn(*mut c_char, usize, *mut usize) -> sys::RiveStatus,
{
    let mut len = 0_usize;
    if call(std::ptr::null_mut(), 0, &mut len) != sys::RIVE_OK {
        return String::new();
    }
    let mut buf = vec![0_u8; len];
    let mut written = 0_usize;
    if call(buf.as_mut_ptr().cast::<c_char>(), buf.len(), &mut written) != sys::RIVE_OK {
        return String::new();
    }
    String::from_utf8_lossy(&buf[..written.min(buf.len())]).into_owned()
}

/// An imported `.riv` file.
///
/// Keeps its [`Context`](crate::Context) alive; `!Send + !Sync`.
pub struct File {
    pub(crate) ptr: *mut sys::RiveFile,
    pub(crate) _ctx: Rc<ContextInner>,
}

impl File {
    /// Instantiates the file's default artboard.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoArtboard`] if the file contains no artboards.
    pub fn default_artboard(&self) -> Result<Artboard> {
        // SAFETY: `self.ptr` is a live file handle.
        self.artboard_from_ptr(unsafe { sys::rive_file_artboard_default(self.ptr) })
    }

    /// Instantiates the artboard with the given `name`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoArtboard`] if the file has no artboard with that name.
    pub fn artboard_named(&self, name: &str) -> Result<Artboard> {
        let c = CString::new(name)
            .map_err(|_| Error::NoArtboard("artboard name contained an interior NUL byte".into()))?;
        // SAFETY: `self.ptr` is a live file handle; `c` is a valid C string.
        self.artboard_from_ptr(unsafe { sys::rive_file_artboard_named(self.ptr, c.as_ptr()) })
    }

    /// Instantiates the artboard at the 0-based `index`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoArtboard`] if `index` is out of range.
    pub fn artboard_at(&self, index: usize) -> Result<Artboard> {
        // SAFETY: `self.ptr` is a live file handle.
        self.artboard_from_ptr(unsafe { sys::rive_file_artboard_at(self.ptr, index as u32) })
    }

    /// Wraps a shim artboard pointer into an [`Artboard`] sharing this file's
    /// context, or maps null to [`Error::NoArtboard`]. Shared by the selectors.
    fn artboard_from_ptr(&self, ptr: *mut sys::RiveArtboard) -> Result<Artboard> {
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

    /// The number of artboards in the file.
    pub fn artboard_count(&self) -> usize {
        // SAFETY: `self.ptr` is a live file handle.
        unsafe { sys::rive_file_artboard_count(self.ptr) as usize }
    }

    /// The names of all artboards in index order — for discovering what
    /// [`artboard_named`](Self::artboard_named) / [`artboard_at`](Self::artboard_at)
    /// can select.
    pub fn artboard_names(&self) -> Vec<String> {
        (0..self.artboard_count())
            .map(|i| {
                // SAFETY: live file handle; `i` < count; the shim's two-call protocol.
                read_name(|buf, cap, out_len| unsafe {
                    sys::rive_file_artboard_name_at(self.ptr, i as u32, buf, cap, out_len)
                })
            })
            .collect()
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
pub(crate) struct ArtboardInner {
    pub(crate) ptr: *mut sys::RiveArtboard,
    /// The owning context. Keeps the device alive *and* identifies which context
    /// this artboard belongs to (checked in `Frame::draw`).
    pub(crate) ctx: Rc<ContextInner>,
}

impl Drop for ArtboardInner {
    fn drop(&mut self) {
        // SAFETY: created by the shim, destroyed exactly once, when the last
        // `Rc<ArtboardInner>` drops — which is after any `StateMachine` built
        // from it has destroyed its scene (it held an `Rc<ArtboardInner>`).
        unsafe { sys::rive_artboard_destroy(self.ptr) };
    }
}

/// An artboard instance, drawable into a [`Frame`](crate::Frame).
///
/// A cheap `Rc` handle: instantiating a [`StateMachine`] shares ownership of the
/// same native artboard, so the artboard outlives the scene that points at it.
/// `!Send + !Sync`.
pub struct Artboard {
    pub(crate) inner: Rc<ArtboardInner>,
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
        self.state_machine_from_ptr(unsafe {
            sys::rive_artboard_state_machine_default(self.inner.ptr)
        })
    }

    /// Instantiates the state machine with the given `name`.
    ///
    /// Unlike [`default_state_machine`](Self::default_state_machine), this never
    /// falls back to an animation/static scene: a missing name is an error.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoStateMachine`] if no state machine has that name.
    pub fn state_machine_named(&self, name: &str) -> Result<StateMachine> {
        let c = CString::new(name).map_err(|_| {
            Error::NoStateMachine("state machine name contained an interior NUL byte".into())
        })?;
        // SAFETY: `self.inner.ptr` is a live artboard handle; `c` is a valid C string.
        self.state_machine_from_ptr(unsafe {
            sys::rive_artboard_state_machine_named(self.inner.ptr, c.as_ptr())
        })
    }

    /// Instantiates the state machine at the 0-based `index` (state machines only;
    /// no animation fallback).
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoStateMachine`] if `index` is out of range.
    pub fn state_machine_at(&self, index: usize) -> Result<StateMachine> {
        // SAFETY: `self.inner.ptr` is a live artboard handle.
        self.state_machine_from_ptr(unsafe {
            sys::rive_artboard_state_machine_at(self.inner.ptr, index as u32)
        })
    }

    /// Wraps a shim state-machine pointer into a [`StateMachine`] keeping this
    /// artboard alive, or maps null to [`Error::NoStateMachine`]. Shared by the
    /// selectors.
    fn state_machine_from_ptr(&self, ptr: *mut sys::RiveStateMachine) -> Result<StateMachine> {
        if ptr.is_null() {
            return Err(Error::NoStateMachine(last_error()));
        }
        Ok(StateMachine {
            ptr,
            _artboard: Rc::clone(&self.inner),
        })
    }

    /// The number of state machines on this artboard.
    pub fn state_machine_count(&self) -> usize {
        // SAFETY: `self.inner.ptr` is a live artboard handle.
        unsafe { sys::rive_artboard_state_machine_count(self.inner.ptr) as usize }
    }

    /// The names of all state machines in index order — for discovering what
    /// [`state_machine_named`](Self::state_machine_named) /
    /// [`state_machine_at`](Self::state_machine_at) can select.
    pub fn state_machine_names(&self) -> Vec<String> {
        (0..self.state_machine_count())
            .map(|i| {
                // SAFETY: live artboard handle; `i` < count; the shim's two-call protocol.
                read_name(|buf, cap, out_len| unsafe {
                    sys::rive_artboard_state_machine_name_at(self.inner.ptr, i as u32, buf, cap, out_len)
                })
            })
            .collect()
    }

    /// Sets how this artboard is scaled/aligned into its draw target (read by the
    /// next [`Frame::draw`](crate::Frame::draw) / draw-viewport). Default
    /// [`FitAlign`] is `Contain` / `Center` (the historical transform), so leaving
    /// it unset renders identically. To keep pointer hits aligned, set the matching
    /// state machine's via [`StateMachine::set_fit_align`] too.
    pub fn set_fit_align(&self, fa: FitAlign) {
        let (fit, ax, ay, scale) = fa.to_raw();
        // SAFETY: `self.inner.ptr` is a live artboard handle.
        unsafe { sys::rive_artboard_set_fit_align(self.inner.ptr, fit, ax, ay, scale) };
    }
}

impl std::fmt::Debug for Artboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Artboard").finish_non_exhaustive()
    }
}

/// Result of forwarding a pointer event to a [`StateMachine`]'s Listeners
/// (mirrors rive's `HitResult`). Tells you how to route the same event to UI
/// behind Rive: `None` — nothing fired, forward it; `Hit` — a listener fired on
/// a transparent shape, forward it too; `HitOpaque` — a listener fired on an
/// opaque shape, Rive consumed the event, don't forward.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum HitResult {
    None = 0,
    Hit = 1,
    HitOpaque = 2,
}

impl HitResult {
    /// Maps the shim's byte to a variant (unknown bytes → [`HitResult::None`]).
    fn from_u8(v: u8) -> Self {
        match v {
            1 => HitResult::Hit,
            2 => HitResult::HitOpaque,
            _ => HitResult::None,
        }
    }
}

/// A state machine (or animation/scene) instance driving an [`Artboard`].
///
/// Holds a shared reference to its [`Artboard`] so the native scene never
/// outlives the artboard instance it points at. `!Send + !Sync`.
pub struct StateMachine {
    // pub(crate) so the per-feature input module (`input.rs`) can drive keyboard /
    // gamepad / focus on the SM handle, mirroring how `rig.rs` reaches Artboard.
    pub(crate) ptr: *mut sys::RiveStateMachine,
    _artboard: Rc<ArtboardInner>,
}

impl StateMachine {
    /// Advances the state machine by `dt_seconds` and applies it to the artboard.
    ///
    /// To **pause**, advance by `0.0` (the artboard is re-applied — pending data
    /// binding still takes effect — but time does not move). For **playback speed**,
    /// scale the step yourself (`advance(dt * speed)`); rive has no per-instance
    /// speed setter (an animation's own speed is baked into the asset).
    pub fn advance(&mut self, dt_seconds: f32) {
        // SAFETY: `self.ptr` is a live state-machine handle.
        unsafe { sys::rive_state_machine_advance(self.ptr, dt_seconds) };
    }

    /// Seeks to absolute `time_seconds` (clamped to `[0, duration]`) and applies it
    /// immediately, so the seeked pose is visible without a following
    /// [`advance`](Self::advance) (e.g. scrubbing while paused).
    ///
    /// Only **linear-animation** scenes are seekable (the default-scene fallback when
    /// an artboard has no state machine). Returns `false` for a state machine — it
    /// has no scalar playhead — leaving it untouched. Use [`duration`](Self::duration)
    /// to test seekability and bound the range.
    pub fn seek(&mut self, time_seconds: f32) -> bool {
        // SAFETY: `self.ptr` is a live state-machine handle.
        unsafe { sys::rive_state_machine_seek(self.ptr, time_seconds) }
    }

    /// The playback duration in seconds, or `None` for a state machine (which is
    /// continuous — no fixed length). `Some` exactly when the scene is seekable.
    pub fn duration(&self) -> Option<f32> {
        // SAFETY: `self.ptr` is a live state-machine handle.
        let d = unsafe { sys::rive_state_machine_duration(self.ptr) };
        (d >= 0.0).then_some(d)
    }

    /// The current playhead position in seconds, or `None` for a state machine.
    pub fn time(&self) -> Option<f32> {
        // SAFETY: `self.ptr` is a live state-machine handle.
        let t = unsafe { sys::rive_state_machine_time(self.ptr) };
        (t >= 0.0).then_some(t)
    }

    /// Sets the [`FitAlign`] used to invert pointer coordinates back into artboard
    /// space. **Must match** the artboard's draw fit/alignment (set via
    /// [`Artboard::set_fit_align`]) or pointer hits won't line up with the rendered
    /// pixels. Default is `Contain` / `Center` (the historical inversion).
    pub fn set_fit_align(&self, fa: FitAlign) {
        let (fit, ax, ay, scale) = fa.to_raw();
        // SAFETY: `self.ptr` is a live state-machine handle.
        unsafe { sys::rive_state_machine_set_fit_align(self.ptr, fit, ax, ay, scale) };
    }

    /// Sets the drawn **tile size** (px) for atlas pointer inversion. An atlas
    /// face draws into a tile sub-rect (via `Frame::draw_viewport`), so its
    /// [`FitAlign`] maps the artboard into the tile, not the full target. With
    /// `tile_w`×`tile_h` set, [`pointer_move`](Self::pointer_move) & co. normalize
    /// the incoming target-space coords into the tile before inverting. Pass
    /// `(0.0, 0.0)` — or any non-positive — to restore full-target inversion (the
    /// dedicated-face default). Pair with a matching [`set_fit_align`](Self::set_fit_align).
    pub fn set_pointer_tile(&self, tile_w: f32, tile_h: f32) {
        // SAFETY: `self.ptr` is a live state-machine handle.
        unsafe { sys::rive_state_machine_set_pointer_tile(self.ptr, tile_w, tile_h) };
    }

    /// Forwards a pointer **move** to the state machine's Listeners. `(x, y)` is
    /// in target-pixel space (`0..w`, `0..h`, top-left origin); `w`×`h` is the
    /// render-target size the coords are relative to. The shim inverts the same
    /// Fit/alignment used to draw, so input lines up with the rendered pixels.
    /// Drives pointer-driven Listeners — eye/head joysticks, hover, etc.
    pub fn pointer_move(&mut self, x: f32, y: f32, w: u32, h: u32) -> HitResult {
        // SAFETY: `self.ptr` is a live state-machine handle.
        HitResult::from_u8(unsafe {
            sys::rive_state_machine_pointer_move(self.ptr, x, y, w as f32, h as f32)
        })
    }

    /// Forwards a pointer **press**. See [`StateMachine::pointer_move`] for the
    /// coordinate contract.
    pub fn pointer_down(&mut self, x: f32, y: f32, w: u32, h: u32) -> HitResult {
        // SAFETY: `self.ptr` is a live state-machine handle.
        HitResult::from_u8(unsafe {
            sys::rive_state_machine_pointer_down(self.ptr, x, y, w as f32, h as f32)
        })
    }

    /// Forwards a pointer **release**. See [`StateMachine::pointer_move`] for the
    /// coordinate contract.
    pub fn pointer_up(&mut self, x: f32, y: f32, w: u32, h: u32) -> HitResult {
        // SAFETY: `self.ptr` is a live state-machine handle.
        HitResult::from_u8(unsafe {
            sys::rive_state_machine_pointer_up(self.ptr, x, y, w as f32, h as f32)
        })
    }

    /// Forwards a pointer **exit** (cursor left the surface). See
    /// [`StateMachine::pointer_move`] for the coordinate contract.
    pub fn pointer_exit(&mut self, x: f32, y: f32, w: u32, h: u32) -> HitResult {
        // SAFETY: `self.ptr` is a live state-machine handle.
        HitResult::from_u8(unsafe {
            sys::rive_state_machine_pointer_exit(self.ptr, x, y, w as f32, h as f32)
        })
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
