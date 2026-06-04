//! The scene graph: an imported [`File`], its [`Artboard`] instances, and the
//! [`StateMachine`] (or animation/scene) that drives one — plus [`HitResult`],
//! the pointer-event outcome. These form one ownership chain (`File` →
//! `Artboard` → `StateMachine`, each keeping the previous alive), so they live
//! together here; the render core (`Context`/`RenderTarget`/`Frame`) stays in
//! `lib.rs`. View-model data binding adds methods to [`Artboard`] in
//! `view_model.rs`.

use std::rc::Rc;

use crate::{last_error, sys, ContextInner, Error, Result};

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
    ptr: *mut sys::RiveStateMachine,
    _artboard: Rc<ArtboardInner>,
}

impl StateMachine {
    /// Advances the state machine by `dt_seconds` and applies it to the artboard.
    pub fn advance(&mut self, dt_seconds: f32) {
        // SAFETY: `self.ptr` is a live state-machine handle.
        unsafe { sys::rive_state_machine_advance(self.ptr, dt_seconds) };
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
