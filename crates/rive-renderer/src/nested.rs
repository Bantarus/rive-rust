//! Runtime **nested-artboard access** — reach into a child artboard mounted by a
//! `NestedArtboard` component on a parent [`Artboard`], by component name or a
//! `/`-delimited path ("child/grandchild").
//!
//! The resolved child is itself an [`Artboard`], so the SAME rig / text / input /
//! view-model accessors drive it — e.g. `parent.nested_artboard("Wheel")?.bone_set(..)`.
//! The child borrows its instance from the parent's `NestedArtboard` component, so
//! the returned handle keeps the parent alive (an `Rc`, enforced at runtime — no
//! lifetime parameter) and is **auto-advanced by the parent**: drive it with the
//! assert-before-advance setters rather than instancing a separate state machine on
//! it. Mirrors the Rive runtime nested-artboard API (https://rive.app/docs).

use std::ffi::CString;
use std::os::raw::c_char;
use std::rc::Rc;

use crate::scene::ArtboardInner;
use crate::{last_error, sys, Artboard, Error, Result};

impl Artboard {
    /// The number of `NestedArtboard` components on this artboard. Works on a
    /// top-level **or** an already-nested handle, so you can descend level by level.
    pub fn nested_artboard_count(&self) -> usize {
        // SAFETY: `self.inner.ptr` is a live artboard handle.
        unsafe { sys::rive_artboard_nested_count(self.inner.ptr) as usize }
    }

    /// The authored names of the nested artboards in order — for discovering what
    /// [`nested_artboard`](Self::nested_artboard) can select in an opaque `.riv`.
    pub fn nested_artboard_names(&self) -> Vec<String> {
        (0..self.nested_artboard_count())
            .map(|i| {
                // SAFETY: live handle; `i` < count; the shim's two-call protocol.
                read_nested_name(|buf, cap, out_len| unsafe {
                    sys::rive_artboard_nested_name_at(self.inner.ptr, i as u32, buf, cap, out_len)
                })
            })
            .collect()
    }

    /// Resolves a nested child by its 0-based `index` in nested order (matching
    /// [`nested_artboard_names`](Self::nested_artboard_names)), returning a borrowed
    /// child [`Artboard`]. Use this when the `NestedArtboard` components are unnamed
    /// (so name lookup can't disambiguate). Same borrowing/advance semantics as
    /// [`nested_artboard`](Self::nested_artboard).
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoArtboard`] if `index` is out of range or the nested
    /// artboard has no mounted instance.
    pub fn nested_artboard_at(&self, index: usize) -> Result<Artboard> {
        // SAFETY: `self.inner.ptr` is a live artboard handle.
        let ptr = unsafe { sys::rive_artboard_nested_at(self.inner.ptr, index as u32) };
        self.wrap_nested(ptr)
    }

    /// Resolves a nested child by its `NestedArtboard` component `name`, returning a
    /// borrowed child [`Artboard`] that keeps this parent alive.
    ///
    /// The child is auto-advanced by the parent's
    /// [`advance`](crate::StateMachine::advance), so drive it with the rig / text /
    /// input setters (assert-before-advance) — calling them on the returned handle
    /// targets the child's components.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoArtboard`] if there is no nested artboard with that name
    /// (or it has no mounted instance), or `name` contained an interior NUL byte.
    pub fn nested_artboard(&self, name: &str) -> Result<Artboard> {
        let c = CString::new(name).map_err(|_| {
            Error::NoArtboard("nested artboard name contained an interior NUL byte".into())
        })?;
        // SAFETY: live parent handle; `c` is a valid C string.
        let ptr = unsafe { sys::rive_artboard_nested_named(self.inner.ptr, c.as_ptr()) };
        self.wrap_nested(ptr)
    }

    /// Resolves a nested child by a `/`-delimited `path` ("child/grandchild") that
    /// descends through nested artboards. Same borrowing/advance semantics as
    /// [`nested_artboard`](Self::nested_artboard).
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoArtboard`] if the path does not resolve (or the resolved
    /// artboard has no instance), or `path` contained an interior NUL byte.
    pub fn nested_artboard_at_path(&self, path: &str) -> Result<Artboard> {
        let c = CString::new(path).map_err(|_| {
            Error::NoArtboard("nested artboard path contained an interior NUL byte".into())
        })?;
        // SAFETY: live parent handle; `c` is a valid C string.
        let ptr = unsafe { sys::rive_artboard_nested_at_path(self.inner.ptr, c.as_ptr()) };
        self.wrap_nested(ptr)
    }

    /// Wraps a borrowed nested-child pointer into an [`Artboard`] that shares this
    /// parent's context and keeps the parent alive (`_parent`), or maps null to
    /// [`Error::NoArtboard`].
    fn wrap_nested(&self, ptr: *mut sys::RiveArtboard) -> Result<Artboard> {
        if ptr.is_null() {
            return Err(Error::NoArtboard(last_error()));
        }
        Ok(Artboard {
            inner: Rc::new(ArtboardInner {
                ptr,
                ctx: Rc::clone(&self.inner.ctx),
                _parent: Some(Rc::clone(&self.inner)),
            }),
        })
    }
}

/// Runs the shim's two-call string protocol (size with a null buffer, then fill),
/// returning the bytes as a `String` (empty on error). For nested-name reads.
fn read_nested_name<F>(call: F) -> String
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
