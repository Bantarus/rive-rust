//! Runtime **rig control** for Bevy — drive a `.riv`'s bones, constraints, and
//! solos at runtime. Attach a [`RiveRig`] to the same entity as
//! [`RiveAnimation`](crate::RiveAnimation) and queue writes; each is applied to
//! the matching component before the next advance (so the state machine / scripts
//! see the change this tick), in BOTH tiers (`floor` inline; `zero_copy` ferried
//! to the render world, like view-model / text writes).
//!
//! Reads (a bone's current transform, a constraint's strength, a solo's active
//! child) are available at the safe layer
//! ([`Artboard::bone_get`](rive_renderer::Artboard::bone_get) etc.); a Bevy
//! read-back channel is deferred (see `docs/feature-support.md`), mirroring the
//! view-model watch read-back.

use bevy::prelude::*;

pub use rive_renderer::BoneProp;

/// One queued rig write, applied to the artboard before the next advance.
#[derive(Clone, Debug)]
pub(crate) enum RigWrite {
    /// Set a bone's transform property.
    Bone {
        name: String,
        prop: BoneProp,
        value: f32,
    },
    /// Set a constraint's strength.
    ConstraintStrength { name: String, value: f32 },
    /// Select a solo's active child by authored name.
    SoloByName { name: String, child: String },
    /// Select a solo's active child by 0-based index.
    SoloByIndex { name: String, index: usize },
}

/// Queues runtime **rig** writes (bones / constraints / solos) for a `.riv`
/// instance. Attach to the same entity as [`RiveAnimation`](crate::RiveAnimation);
/// each queued write is applied to the matching component (resolved by authored
/// name) before the next advance. Honored in both tiers.
///
/// A write takes effect on the next advance/draw and "sticks" only if the active
/// animation does not *also* key that property — the same assert-before-advance
/// contract as the view-model / text writes. For procedural control (e.g. aiming
/// a bone at a cursor) re-queue the write each frame.
#[derive(Component, Default, Debug)]
pub struct RiveRig {
    /// Pending writes, drained + applied before each advance.
    writes: Vec<RigWrite>,
    /// `zero_copy` double-buffer: `writes` are moved here (main world) so the
    /// read-only extract can ferry them to the render world, then cleared the
    /// following frame. Absent under `floor` (it drains `writes` inline).
    #[cfg(feature = "zero_copy")]
    staged: Vec<RigWrite>,
}

impl RiveRig {
    /// Queues a set of transform property `prop` on the bone named `name`
    /// (`BoneProp::X`/`Y` require a root bone).
    pub fn set_bone(&mut self, name: impl Into<String>, prop: BoneProp, value: f32) {
        self.writes.push(RigWrite::Bone {
            name: name.into(),
            prop,
            value,
        });
    }

    /// Queues a set of the strength (typically `0.0..=1.0`) of the constraint
    /// named `name`.
    pub fn set_constraint_strength(&mut self, name: impl Into<String>, value: f32) {
        self.writes.push(RigWrite::ConstraintStrength {
            name: name.into(),
            value,
        });
    }

    /// Queues a select of the active child (by authored name) of the solo named
    /// `name`.
    pub fn set_solo_active(&mut self, name: impl Into<String>, child: impl Into<String>) {
        self.writes.push(RigWrite::SoloByName {
            name: name.into(),
            child: child.into(),
        });
    }

    /// Queues a select of the active child (by 0-based index) of the solo named
    /// `name`.
    pub fn set_solo_active_index(&mut self, name: impl Into<String>, index: usize) {
        self.writes.push(RigWrite::SoloByIndex {
            name: name.into(),
            index,
        });
    }

    /// `zero_copy`: whether there is staging work (queued or staged writes).
    #[cfg(feature = "zero_copy")]
    pub(crate) fn has_staging_work(&self) -> bool {
        !self.writes.is_empty() || !self.staged.is_empty()
    }

    /// `zero_copy`: move this frame's queued writes into the staging buffer (or
    /// clear last frame's), so the read-only extract can ferry them.
    #[cfg(feature = "zero_copy")]
    pub(crate) fn stage_writes(&mut self) {
        if self.writes.is_empty() {
            self.staged.clear();
        } else {
            self.staged = std::mem::take(&mut self.writes);
        }
    }

    /// `zero_copy`: the writes staged for this frame (ferried by extract).
    #[cfg(feature = "zero_copy")]
    pub(crate) fn staged(&self) -> &[RigWrite] {
        &self.staged
    }
}

/// Applies a slice of rig writes to the artboard. Shared by both tiers (`floor`
/// drains inline; `zero_copy` ferries a slice to the render world). Per-write
/// failures `warn!` and continue.
#[cfg(any(feature = "floor", feature = "zero_copy"))]
pub(crate) fn apply_rig_writes_slice(artboard: &rive_renderer::Artboard, writes: &[RigWrite]) {
    for w in writes {
        let result = match w {
            RigWrite::Bone { name, prop, value } => artboard.bone_set(name, *prop, *value),
            RigWrite::ConstraintStrength { name, value } => {
                artboard.constraint_set_strength(name, *value)
            }
            RigWrite::SoloByName { name, child } => artboard.solo_set_active(name, child),
            RigWrite::SoloByIndex { name, index } => artboard.solo_set_active_index(name, *index),
        };
        if let Err(e) = result {
            warn!("rive: rig write {w:?} failed: {e}");
        }
    }
}

/// Drains queued rig writes to the artboard. Call **before** advancing so the
/// change is solved + visible this tick.
#[cfg(feature = "floor")]
pub(crate) fn apply_rig_writes(rig: &mut RiveRig, artboard: &rive_renderer::Artboard) {
    let writes = std::mem::take(&mut rig.writes);
    apply_rig_writes_slice(artboard, &writes);
}
