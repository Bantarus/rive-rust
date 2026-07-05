//! Runtime **rig control** for Bevy — drive a `.riv`'s bones, constraints, and
//! solos at runtime. Attach a [`RiveRig`] to the same entity as
//! [`RiveAnimation`](crate::RiveAnimation) and queue writes; each is applied to
//! the matching component before the next advance (so the state machine / scripts
//! see the change this tick), in BOTH tiers (`floor` inline; `zero_copy` ferried
//! to the render world, like view-model / text writes).
//!
//! Reads (a bone's current transform, a constraint's strength, a solo's active
//! child) ride the same **register-then-read-back** model as the view model's
//! `watch_*`: call [`RiveRig::watch_bone`] / [`watch_constraint_strength`](RiveRig::watch_constraint_strength)
//! / [`watch_constraint_prop`](RiveRig::watch_constraint_prop) / [`watch_solo`](RiveRig::watch_solo),
//! then read the last value with [`bone`](RiveRig::bone) / [`constraint_strength`](RiveRig::constraint_strength)
//! / [`constraint_prop`](RiveRig::constraint_prop) / [`solo_active`](RiveRig::solo_active).
//! Refreshed after each advance in BOTH tiers — `floor` inline (same frame),
//! `zero_copy` over the render→main back-channel (`RiveReadbackChannel`; one frame
//! of latency, like the view-model watch read-back).

use std::collections::HashMap;

use bevy::prelude::*;

pub use rive_renderer::{BoneProp, ConstraintProp};

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
    /// Set a type-specific constraint property (IK / distance / follow-path).
    ConstraintProp {
        name: String,
        prop: ConstraintProp,
        value: f32,
    },
    /// Select a solo's active child by authored name.
    SoloByName { name: String, child: String },
    /// Select a solo's active child by 0-based index.
    SoloByIndex { name: String, index: usize },
}

/// One registered rig **read**, refreshed into [`RiveRig`]'s read-back store after
/// each advance (the read analogue of [`RigWrite`]). `PartialEq`/`Eq` so the drain
/// can confirm a read is still registered before delivering its value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RigRead {
    /// Read a bone's transform property.
    Bone { name: String, prop: BoneProp },
    /// Read a constraint's strength.
    ConstraintStrength { name: String },
    /// Read a type-specific constraint property (IK / distance / follow-path).
    ConstraintProp { name: String, prop: ConstraintProp },
    /// Read a solo's active child (index + name).
    Solo { name: String },
}

/// A rig read-back value produced by [`read_rig_slice`]. Kept `pub(crate)`: the
/// public accessors ([`RiveRig::bone`] etc.) return the plain `f32` / index / name,
/// so this enum never crosses the API boundary — it only tags the internal store.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RigValue {
    /// A bone transform prop, constraint strength, or constraint prop (all `f32`).
    Number(f32),
    /// A solo's active child: its 0-based `index` (`None` when nothing is active)
    /// and authored `name` (empty when nothing is active).
    Solo { index: Option<usize>, name: String },
}

/// The internal read-back store key for `read`. A single source of truth shared by
/// the `watch_*`/accessor pairs (via the `*_key` helpers below) and the drain, so a
/// value stored under one key is always found under the same key.
fn rig_key(read: &RigRead) -> String {
    match read {
        RigRead::Bone { name, prop } => bone_key(name, *prop),
        RigRead::ConstraintStrength { name } => cstrength_key(name),
        RigRead::ConstraintProp { name, prop } => cprop_key(name, *prop),
        RigRead::Solo { name } => solo_key(name),
    }
}

// The per-kind key builders. The kind prefix (`b`/`cs`/`cp`/`s`) plus the numeric
// prop keeps kinds disjoint; the authored `name` is the tail, so it can contain any
// character without colliding across kinds.
fn bone_key(name: &str, prop: BoneProp) -> String {
    format!("b{}:{name}", prop as u32)
}
fn cstrength_key(name: &str) -> String {
    format!("cs:{name}")
}
fn cprop_key(name: &str, prop: ConstraintProp) -> String {
    format!("cp{}:{name}", prop as u32)
}
fn solo_key(name: &str) -> String {
    format!("s:{name}")
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
    /// Registered reads (bones / constraints / solos), refreshed into `values`
    /// after each advance. A persistent registration (not drained) — ferried each
    /// frame to the render world under `zero_copy`, like the view-model watch list.
    reads: Vec<RigRead>,
    /// Latest read-back of each registered read, keyed by [`rig_key`]. Written after
    /// advance (`floor` inline; `zero_copy` from the drain); read by the accessors.
    /// A culled face keeps its last read-back (no advance ⇒ no refresh).
    values: HashMap<String, RigValue>,
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

    /// Queues a set of a **type-specific** constraint property (e.g. an IK
    /// constraint's invert flag, a distance constraint's mode) on the constraint
    /// named `name`. Booleans pass `0.0`/`1.0`; see [`ConstraintProp`] for the
    /// per-field encoding.
    pub fn set_constraint_prop(
        &mut self,
        name: impl Into<String>,
        prop: ConstraintProp,
        value: f32,
    ) {
        self.writes.push(RigWrite::ConstraintProp {
            name: name.into(),
            prop,
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

    /// Registers a **bone** transform property to read back into this component
    /// after each advance. Read it with [`bone`](Self::bone). Idempotent.
    /// `BoneProp::X`/`Y` require a **root bone** (like [`set_bone`](Self::set_bone))
    /// — on a regular bone the read fails each frame (warns; the accessor stays
    /// at its last value, `None` if it never succeeded).
    pub fn watch_bone(&mut self, name: impl Into<String>, prop: BoneProp) {
        self.add_read(RigRead::Bone {
            name: name.into(),
            prop,
        });
    }

    /// Registers a **constraint strength** to read back each advance. Read it with
    /// [`constraint_strength`](Self::constraint_strength). Idempotent.
    pub fn watch_constraint_strength(&mut self, name: impl Into<String>) {
        self.add_read(RigRead::ConstraintStrength { name: name.into() });
    }

    /// Registers a **type-specific constraint property** to read back each advance.
    /// Read it with [`constraint_prop`](Self::constraint_prop). Idempotent.
    pub fn watch_constraint_prop(&mut self, name: impl Into<String>, prop: ConstraintProp) {
        self.add_read(RigRead::ConstraintProp {
            name: name.into(),
            prop,
        });
    }

    /// Registers a **solo**'s active child to read back each advance. Read it with
    /// [`solo_active`](Self::solo_active) / [`solo_active_index`](Self::solo_active_index).
    /// Idempotent.
    pub fn watch_solo(&mut self, name: impl Into<String>) {
        self.add_read(RigRead::Solo { name: name.into() });
    }

    fn add_read(&mut self, read: RigRead) {
        if !self.reads.contains(&read) {
            self.reads.push(read);
        }
    }

    /// Last read-back rotation/scale/length/translation of the bone named `name`
    /// (if [watched](Self::watch_bone); x/y are root-bone-only — see
    /// [`watch_bone`](Self::watch_bone)). Reflects the pose *after* the last
    /// advance — the value the state machine / constraints solved to (one frame
    /// late under `zero_copy`). `None` until the first read-back lands; a read
    /// that later starts failing warns and keeps the last successful value.
    pub fn bone(&self, name: &str, prop: BoneProp) -> Option<f32> {
        self.number(&bone_key(name, prop))
    }

    /// Last read-back strength of the constraint named `name` (if
    /// [watched](Self::watch_constraint_strength)).
    pub fn constraint_strength(&self, name: &str) -> Option<f32> {
        self.number(&cstrength_key(name))
    }

    /// Last read-back type-specific property of the constraint named `name` (if
    /// [watched](Self::watch_constraint_prop)). Booleans read back as `0.0`/`1.0`.
    pub fn constraint_prop(&self, name: &str, prop: ConstraintProp) -> Option<f32> {
        self.number(&cprop_key(name, prop))
    }

    /// Last read-back **0-based index** of the active child of the solo named `name`
    /// (if [watched](Self::watch_solo)). `None` when nothing is active (or no
    /// read-back yet).
    pub fn solo_active_index(&self, name: &str) -> Option<usize> {
        match self.values.get(&solo_key(name)) {
            Some(RigValue::Solo { index, .. }) => *index,
            _ => None,
        }
    }

    /// Last read-back **authored name** of the active child of the solo named `name`
    /// (if [watched](Self::watch_solo)). `None` when nothing is active (or no
    /// read-back yet).
    pub fn solo_active(&self, name: &str) -> Option<&str> {
        match self.values.get(&solo_key(name)) {
            Some(RigValue::Solo { name, .. }) if !name.is_empty() => Some(name),
            _ => None,
        }
    }

    /// Shared getter for the `Number`-valued reads (bone / constraint).
    fn number(&self, key: &str) -> Option<f32> {
        match self.values.get(key) {
            Some(RigValue::Number(n)) => Some(*n),
            _ => None,
        }
    }

    /// Whether any rig read is registered — gates the `floor` advance loop's
    /// post-advance nested re-resolve + refresh (checked through `Deref`, so an
    /// unwatched face doesn't trip change detection).
    #[cfg(feature = "floor")]
    pub(crate) fn has_reads(&self) -> bool {
        !self.reads.is_empty()
    }

    /// `zero_copy`: the registered read list, ferried by extract to the render world
    /// where the node reads it after advance and ships results back over the channel.
    #[cfg(feature = "zero_copy")]
    pub(crate) fn read_list(&self) -> &[RigRead] {
        &self.reads
    }

    /// `zero_copy`: store one read-back delivered by the drain, but only if `read`
    /// is still registered — the value was produced from LAST frame's reads and the
    /// component may have dropped it since (mirrors the view-model drain's guard).
    #[cfg(feature = "zero_copy")]
    pub(crate) fn store_read(&mut self, read: &RigRead, value: RigValue) {
        if self.reads.contains(read) {
            self.values.insert(rig_key(read), value);
        }
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
            RigWrite::ConstraintProp { name, prop, value } => {
                artboard.constraint_set_prop(name, *prop, *value)
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

/// Reads each registered rig read, returning the successful `(read, value)` pairs.
/// Shared by both tiers (`floor` stores them into [`RiveRig`] inline via
/// [`refresh_rig_reads`]; `zero_copy` ships them back over the render→main channel).
/// Call **after** advancing so reads reflect this tick's solved pose. Per-read
/// failures `warn!` and continue — NOTE a read is a persistent registration, so a
/// bad name/prop warns every frame (deliberately matching the view-model
/// `read_watch_slice` convention; fix the registration, don't mute the log).
#[cfg(any(feature = "floor", feature = "zero_copy"))]
pub(crate) fn read_rig_slice(
    artboard: &rive_renderer::Artboard,
    reads: &[RigRead],
) -> Vec<(RigRead, RigValue)> {
    let mut out = Vec::new();
    for read in reads {
        let value = match read {
            RigRead::Bone { name, prop } => artboard.bone_get(name, *prop).map(RigValue::Number),
            RigRead::ConstraintStrength { name } => {
                artboard.constraint_get_strength(name).map(RigValue::Number)
            }
            RigRead::ConstraintProp { name, prop } => {
                artboard.constraint_get_prop(name, *prop).map(RigValue::Number)
            }
            // Active-child name is the fallible read (validates the solo name); the
            // index reads `None` on its own for "nothing active", so pair them.
            RigRead::Solo { name } => artboard.solo_get_active(name).map(|child| RigValue::Solo {
                index: artboard.solo_get_active_index(name),
                name: child,
            }),
        };
        match value {
            Ok(v) => out.push((read.clone(), v)),
            Err(e) => warn!("rive: rig read {read:?} failed: {e}"),
        }
    }
    out
}

/// Refreshes registered rig reads into [`RiveRig`]'s read-back store. Call **after**
/// advancing so reads reflect this tick's solved pose. (The `floor` wrapper over
/// [`read_rig_slice`], writing the component's `values` inline.)
#[cfg(feature = "floor")]
pub(crate) fn refresh_rig_reads(rig: &mut RiveRig, artboard: &rive_renderer::Artboard) {
    if rig.reads.is_empty() {
        return;
    }
    // Take `reads` out so `values` can be mutated without aliasing it.
    let reads = std::mem::take(&mut rig.reads);
    for (read, value) in read_rig_slice(artboard, &reads) {
        rig.values.insert(rig_key(&read), value);
    }
    rig.reads = reads;
}
