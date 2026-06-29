//! Runtime **rig control** — drive bones, constraints, and solos on an
//! [`Artboard`] by authored component name. Each setter is asserted on the
//! artboard and takes effect on the next [`advance`](crate::StateMachine::advance)
//! / draw (advance solves on top, so a written value sticks only if the active
//! animation does not *also* key that property — the same "assert before advance"
//! contract as the view-model / text writes). Introspection
//! ([`bone_names`](Artboard::bone_names) etc.) lists the components a game can
//! address in an opaque `.riv`.
//!
//! Mirrors the Rive runtime API (https://rive.app/docs). The methods extend
//! [`Artboard`] (defined in `scene.rs`), alongside the `vm_*` / `text_*` accessors.

use std::ffi::CString;
use std::os::raw::c_char;

use crate::{last_error, sys, Artboard, Error, Result};

/// Which transform property of a bone to read or write. `Rotation` (degrees),
/// `ScaleX`, `ScaleY`, and `Length` apply to **any** bone; `X` and `Y` apply to
/// **root bones only** (a regular bone's position is derived from its parent
/// chain) — addressing `X`/`Y` on a non-root bone is an [`Error::Rig`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum BoneProp {
    /// Local rotation, in degrees (any bone).
    Rotation = 0,
    /// Horizontal scale factor (any bone).
    ScaleX = 1,
    /// Vertical scale factor (any bone).
    ScaleY = 2,
    /// Bone length (any bone).
    Length = 3,
    /// Local X translation — **root bones only**.
    X = 4,
    /// Local Y translation — **root bones only**.
    Y = 5,
}

/// A **type-specific** constraint property — the field is only valid on the
/// matching concrete constraint type, so addressing it on a constraint of a
/// different type (or a missing name) is an [`Error::Rig`]. The universal
/// `strength` knob is separate ([`constraint_set_strength`](Artboard::constraint_set_strength)).
///
/// Values ride a single `f32` channel: booleans are `0.0`/`1.0` and the distance
/// [`DistanceMode`](Self::DistanceMode) is its 0-based index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum ConstraintProp {
    /// `IKConstraint::invertDirection` — bool (`0.0`/`1.0`).
    IkInvert = 0,
    /// `IKConstraint::parentBoneCount` — how many parent bones the IK solves over
    /// (rounded to a non-negative integer).
    IkParentBoneCount = 1,
    /// `DistanceConstraint::distance` — the target distance (f32).
    DistanceDistance = 2,
    /// `DistanceConstraint::mode` — `0` = Closer (≤), `1` = Further (≥), `2` = Exact.
    DistanceMode = 3,
    /// `FollowPathConstraint::distance` — position along the path (`0.0`..=`1.0`).
    FollowPathDistance = 4,
    /// `FollowPathConstraint::orient` — orient toward the path direction; bool.
    FollowPathOrient = 5,
    /// `FollowPathConstraint::offset` — apply the component's local offset; bool.
    FollowPathOffset = 6,
}

/// Component kind for the generalized introspection ABI (mirrors `RIVE_RIG_*`).
#[repr(u32)]
enum RigKind {
    Bone = 0,
    Constraint = 1,
    Solo = 2,
}

/// Maps an interior-NUL failure on a rig component name to [`Error::Rig`].
fn rig_cstring(s: &str, what: &str) -> Result<CString> {
    CString::new(s).map_err(|_| Error::Rig(format!("{what} contained an interior NUL byte")))
}

/// `RIVE_OK` → `Ok(())`, otherwise the shim's last error as [`Error::Rig`].
fn rig_status(st: sys::RiveStatus) -> Result<()> {
    if st == sys::RIVE_OK {
        Ok(())
    } else {
        Err(Error::Rig(last_error()))
    }
}

/// Runs the shim's two-call string protocol (size with a null buffer, then fill)
/// via `call`, returning the bytes as a `String`. `call(buf, cap, out_len)`.
fn read_string_via<F>(call: F) -> Result<String>
where
    F: Fn(*mut c_char, usize, *mut usize) -> sys::RiveStatus,
{
    let mut len = 0_usize;
    rig_status(call(std::ptr::null_mut(), 0, &mut len))?;
    let mut buf = vec![0_u8; len];
    let mut written = 0_usize;
    rig_status(call(buf.as_mut_ptr().cast::<c_char>(), buf.len(), &mut written))?;
    Ok(String::from_utf8_lossy(&buf[..written.min(buf.len())]).into_owned())
}

impl Artboard {
    /// Sets transform property `prop` of the bone named `name`. `X`/`Y` require a
    /// root bone.
    ///
    /// # Errors
    ///
    /// [`Error::Rig`] if no such bone exists, `prop` is `X`/`Y` on a non-root
    /// bone, or `name` contained an interior NUL byte.
    pub fn bone_set(&self, name: &str, prop: BoneProp, value: f32) -> Result<()> {
        let name_c = rig_cstring(name, "bone name")?;
        // SAFETY: live artboard handle; `name_c` is a valid C string.
        let st = unsafe {
            sys::rive_artboard_bone_set(self.inner.ptr, name_c.as_ptr(), prop as u32, value)
        };
        rig_status(st)
    }

    /// Reads transform property `prop` of the bone named `name`. `X`/`Y` require a
    /// root bone.
    ///
    /// # Errors
    ///
    /// [`Error::Rig`] if no such bone exists, `prop` is `X`/`Y` on a non-root
    /// bone, or `name` contained an interior NUL byte.
    pub fn bone_get(&self, name: &str, prop: BoneProp) -> Result<f32> {
        let name_c = rig_cstring(name, "bone name")?;
        let mut out = 0.0_f32;
        // SAFETY: live artboard handle; `name_c` valid; `out` is a live f32.
        let st = unsafe {
            sys::rive_artboard_bone_get(self.inner.ptr, name_c.as_ptr(), prop as u32, &mut out)
        };
        rig_status(st).map(|()| out)
    }

    /// Sets the strength (typically `0.0..=1.0`) of the constraint named `name`.
    ///
    /// # Errors
    ///
    /// [`Error::Rig`] if no such constraint exists, or `name` contained an
    /// interior NUL byte.
    pub fn constraint_set_strength(&self, name: &str, value: f32) -> Result<()> {
        let name_c = rig_cstring(name, "constraint name")?;
        // SAFETY: live artboard handle; `name_c` is a valid C string.
        let st = unsafe {
            sys::rive_artboard_constraint_set_strength(self.inner.ptr, name_c.as_ptr(), value)
        };
        rig_status(st)
    }

    /// Reads the strength of the constraint named `name`.
    ///
    /// # Errors
    ///
    /// [`Error::Rig`] if no such constraint exists, or `name` contained an
    /// interior NUL byte.
    pub fn constraint_get_strength(&self, name: &str) -> Result<f32> {
        let name_c = rig_cstring(name, "constraint name")?;
        let mut out = 0.0_f32;
        // SAFETY: live artboard handle; `name_c` valid; `out` is a live f32.
        let st = unsafe {
            sys::rive_artboard_constraint_get_strength(self.inner.ptr, name_c.as_ptr(), &mut out)
        };
        rig_status(st).map(|()| out)
    }

    /// Sets a **type-specific** constraint property `prop` on the constraint named
    /// `name` (e.g. an IK constraint's invert flag, a distance constraint's mode).
    /// Booleans pass `0.0`/`1.0`; the distance mode passes its 0-based index. See
    /// [`ConstraintProp`] for the per-field encoding.
    ///
    /// # Errors
    ///
    /// [`Error::Rig`] if no constraint of the matching type has that `name`, or
    /// `name` contained an interior NUL byte.
    pub fn constraint_set_prop(&self, name: &str, prop: ConstraintProp, value: f32) -> Result<()> {
        let name_c = rig_cstring(name, "constraint name")?;
        // SAFETY: live artboard handle; `name_c` is a valid C string.
        let st = unsafe {
            sys::rive_artboard_constraint_set_prop(
                self.inner.ptr,
                name_c.as_ptr(),
                prop as u32,
                value,
            )
        };
        rig_status(st)
    }

    /// Reads a **type-specific** constraint property `prop` of the constraint named
    /// `name`. Booleans read back as `0.0`/`1.0`; the distance mode as its index.
    ///
    /// # Errors
    ///
    /// [`Error::Rig`] if no constraint of the matching type has that `name`, or
    /// `name` contained an interior NUL byte.
    pub fn constraint_get_prop(&self, name: &str, prop: ConstraintProp) -> Result<f32> {
        let name_c = rig_cstring(name, "constraint name")?;
        let mut out = 0.0_f32;
        // SAFETY: live artboard handle; `name_c` valid; `out` is a live f32.
        let st = unsafe {
            sys::rive_artboard_constraint_get_prop(
                self.inner.ptr,
                name_c.as_ptr(),
                prop as u32,
                &mut out,
            )
        };
        rig_status(st).map(|()| out)
    }

    /// Sets the active child (by authored name) of the solo named `name`
    /// (exclusive visibility — only the selected child renders).
    ///
    /// `child` must name a member of the solo set; naming a child that the solo
    /// excludes from that set (a constraint or clipping shape) is accepted by the
    /// runtime but has no visible effect.
    ///
    /// # Errors
    ///
    /// [`Error::Rig`] if no such solo / child exists, or a name contained an
    /// interior NUL byte.
    pub fn solo_set_active(&self, name: &str, child: &str) -> Result<()> {
        let name_c = rig_cstring(name, "solo name")?;
        let child_c = rig_cstring(child, "solo child name")?;
        // SAFETY: live artboard handle; both are valid C strings.
        let st = unsafe {
            sys::rive_artboard_solo_set_active_name(
                self.inner.ptr,
                name_c.as_ptr(),
                child_c.as_ptr(),
            )
        };
        rig_status(st)
    }

    /// Sets the active child (by 0-based index) of the solo named `name`.
    ///
    /// # Errors
    ///
    /// [`Error::Rig`] if no such solo exists, `index` is out of range, or `name`
    /// contained an interior NUL byte.
    pub fn solo_set_active_index(&self, name: &str, index: usize) -> Result<()> {
        let name_c = rig_cstring(name, "solo name")?;
        // SAFETY: live artboard handle; `name_c` is a valid C string.
        let st = unsafe {
            sys::rive_artboard_solo_set_active_index(self.inner.ptr, name_c.as_ptr(), index as u32)
        };
        rig_status(st)
    }

    /// The authored name of the solo's currently active child (empty if none is
    /// active).
    ///
    /// # Errors
    ///
    /// [`Error::Rig`] if no such solo exists, or `name` contained an interior NUL
    /// byte.
    pub fn solo_get_active(&self, name: &str) -> Result<String> {
        let name_c = rig_cstring(name, "solo name")?;
        // SAFETY: live artboard handle; `name_c` valid; two-call protocol.
        read_string_via(|buf, cap, out_len| unsafe {
            sys::rive_artboard_solo_get_active_name(
                self.inner.ptr,
                name_c.as_ptr(),
                buf,
                cap,
                out_len,
            )
        })
    }

    /// The 0-based index of the solo's currently active child, or `None` if none
    /// is active or the solo / name is invalid.
    pub fn solo_get_active_index(&self, name: &str) -> Option<usize> {
        let name_c = rig_cstring(name, "solo name").ok()?;
        // SAFETY: live artboard handle; `name_c` is a valid C string.
        let i = unsafe { sys::rive_artboard_solo_get_active_index(self.inner.ptr, name_c.as_ptr()) };
        (i >= 0).then_some(i as usize)
    }

    /// The authored names of all bones (including root bones) on the artboard —
    /// for discovering what [`bone_set`](Self::bone_set) can address.
    pub fn bone_names(&self) -> Vec<String> {
        self.rig_names(RigKind::Bone)
    }

    /// The authored names of all constraints on the artboard — for discovering
    /// what [`constraint_set_strength`](Self::constraint_set_strength) addresses.
    pub fn constraint_names(&self) -> Vec<String> {
        self.rig_names(RigKind::Constraint)
    }

    /// The authored names of all solos on the artboard — for discovering what
    /// [`solo_set_active`](Self::solo_set_active) addresses.
    pub fn solo_names(&self) -> Vec<String> {
        self.rig_names(RigKind::Solo)
    }

    /// Shared introspection: names of all rig components of `kind`, in artboard
    /// object order.
    fn rig_names(&self, kind: RigKind) -> Vec<String> {
        let kind = kind as u32;
        // SAFETY: `self.inner.ptr` is a live artboard handle.
        let count = unsafe { sys::rive_artboard_rig_count(self.inner.ptr, kind) };
        (0..count)
            .map(|i| {
                // SAFETY: live handle; `i` < count; the shim's two-call protocol.
                read_string_via(|buf, cap, out_len| unsafe {
                    sys::rive_artboard_rig_name_at(self.inner.ptr, kind, i, buf, cap, out_len)
                })
                .unwrap_or_default()
            })
            .collect()
    }
}
