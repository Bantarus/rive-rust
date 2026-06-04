//! View-model **data binding** for Bevy — the game↔face data channel. Attach a
//! [`RiveViewModel`] to the same entity as `RiveAnimation`: queue **writes**
//! (applied before each advance, so the state machine / script sees them this
//! tick) and register **watch** paths (read back into [`RiveViewModel::values`]
//! after each advance, so the game observes script output). Property paths use
//! `/` for nested view models, e.g. `"breath/scaleX"`. Mirrors the C++ contract
//! in `docs/cpp/data-binding.mdx`.
//!
//! Slice 1 wires number / bool / trigger. Color, string and enum follow (tracked
//! in `docs/feature-support.md`). The component itself is tier-agnostic; the
//! apply/read helpers are `floor`-only for now (the `zero_copy` tier will reuse
//! the component and call the same `rive_renderer::Artboard` methods).

use std::collections::HashMap;

use bevy::prelude::*;

/// A typed view-model value — produced by writes and stored in read-back.
#[derive(Debug, Clone, PartialEq)]
pub enum RiveValue {
    /// 32-bit number.
    Number(f32),
    /// Boolean.
    Bool(bool),
    /// One-shot trigger (write-only).
    Trigger,
}

/// Which typed getter to use when refreshing a watched path (the property's type
/// isn't introspected for nested paths in slice 1, so the caller declares it via
/// [`RiveViewModel::watch_number`] / [`RiveViewModel::watch_bool`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatchKind {
    Number,
    Bool,
}

/// Read/write a face's view-model properties. Spawn alongside `RiveAnimation`.
#[derive(Component, Default, Debug)]
pub struct RiveViewModel {
    /// Pending writes, drained + applied before each advance.
    writes: Vec<(String, RiveValue)>,
    /// Paths refreshed into `values` after each advance, with how to read each.
    watch: Vec<(String, WatchKind)>,
    /// Latest read-back of each watched path (refreshed every frame).
    pub values: HashMap<String, RiveValue>,
}

impl RiveViewModel {
    /// Queues a write to a **number** property (applied before the next advance).
    pub fn set_number(&mut self, path: impl Into<String>, value: f32) {
        self.writes.push((path.into(), RiveValue::Number(value)));
    }

    /// Queues a write to a **bool** property.
    pub fn set_bool(&mut self, path: impl Into<String>, value: bool) {
        self.writes.push((path.into(), RiveValue::Bool(value)));
    }

    /// Queues a one-shot **trigger** fire.
    pub fn fire_trigger(&mut self, path: impl Into<String>) {
        self.writes.push((path.into(), RiveValue::Trigger));
    }

    /// Registers a **number** property to read back into [`Self::values`] each
    /// frame. Idempotent.
    pub fn watch_number(&mut self, path: impl Into<String>) {
        self.add_watch(path.into(), WatchKind::Number);
    }

    /// Registers a **bool** property to read back each frame. Idempotent.
    pub fn watch_bool(&mut self, path: impl Into<String>) {
        self.add_watch(path.into(), WatchKind::Bool);
    }

    fn add_watch(&mut self, path: String, kind: WatchKind) {
        if !self.watch.iter().any(|(p, _)| *p == path) {
            self.watch.push((path, kind));
        }
    }

    /// Last read-back **number** for `path` (if watched as a number).
    pub fn number(&self, path: &str) -> Option<f32> {
        match self.values.get(path) {
            Some(RiveValue::Number(n)) => Some(*n),
            _ => None,
        }
    }

    /// Last read-back **bool** for `path` (if watched as a bool).
    pub fn boolean(&self, path: &str) -> Option<bool> {
        match self.values.get(path) {
            Some(RiveValue::Bool(b)) => Some(*b),
            _ => None,
        }
    }
}

// ---- floor-tier apply/read (operate on the native rive_renderer::Artboard) ----

/// Drains queued writes to the artboard's bound view model. Call **before**
/// advancing so the state machine / scripts observe them this tick.
#[cfg(feature = "floor")]
pub(crate) fn apply_writes(vm: &mut RiveViewModel, artboard: &rive_renderer::Artboard) {
    for (path, value) in vm.writes.drain(..) {
        let res = match value {
            RiveValue::Number(n) => artboard.vm_set_number(&path, n),
            RiveValue::Bool(b) => artboard.vm_set_bool(&path, b),
            RiveValue::Trigger => artboard.vm_fire_trigger(&path),
        };
        if let Err(e) = res {
            warn!("rive: view-model write {path:?} failed: {e}");
        }
    }
}

/// Refreshes watched paths into [`RiveViewModel::values`]. Call **after**
/// advancing so reads reflect this tick's script / state-machine output.
#[cfg(feature = "floor")]
pub(crate) fn refresh_watch(vm: &mut RiveViewModel, artboard: &rive_renderer::Artboard) {
    if vm.watch.is_empty() {
        return;
    }
    // Take the watch list out to avoid borrowing `vm` while writing `vm.values`.
    let watch = std::mem::take(&mut vm.watch);
    for (path, kind) in &watch {
        let read = match kind {
            WatchKind::Number => artboard.vm_get_number(path).map(RiveValue::Number),
            WatchKind::Bool => artboard.vm_get_bool(path).map(RiveValue::Bool),
        };
        match read {
            Ok(v) => {
                vm.values.insert(path.clone(), v);
            }
            Err(e) => warn!("rive: view-model read {path:?} failed: {e}"),
        }
    }
    vm.watch = watch;
}
