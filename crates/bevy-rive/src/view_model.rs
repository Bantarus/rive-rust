//! View-model **data binding** for Bevy — the game↔face data channel. Attach a
//! [`RiveViewModel`] to the same entity as `RiveAnimation`:
//! - queue **writes** (applied before each advance, so the state machine / script
//!   sees them this tick);
//! - register **watch** paths (read back into [`RiveViewModel::values`] after each
//!   advance, so the game observes script output);
//! - register **observe** paths ([`RiveViewModel::observe`]) — each frame the rig
//!   changes that property, or fires that trigger, a [`RivePropertyChanged`] message
//!   is emitted. This is the modern, non-deprecated replacement for events
//!   read-back (Rive deprecated runtime *event* listening; the rig signals gameplay
//!   by driving a view-model trigger/property instead).
//!
//! Property paths use `/` for nested view models, e.g. `"breath/scaleX"`. Mirrors
//! the C++ contract in the Rive data-binding docs (https://rive.app/docs).
//!
//! Covers number / bool / trigger / color / string / enum. The component is
//! tier-agnostic. **Writes** are forwarded in both tiers: the `floor` advance
//! system applies them inline; the `zero_copy` tier ferries them to the render
//! world (where its instances live) via a per-frame staging buffer. **Watch
//! read-back** AND **observe** (change/trigger fires) are `floor`-only for now —
//! `zero_copy` advances in the render world and a render→main back-channel for
//! reads is deferred (see `docs/feature-support.md`); registering a watch/observe
//! under `zero_copy` is a no-op.

use std::collections::HashMap;

use bevy::prelude::*;

/// A typed view-model value — produced by writes and stored in read-back.
#[derive(Debug, Clone, PartialEq)]
pub enum RiveValue {
    /// 32-bit number.
    Number(f32),
    /// Boolean.
    Bool(bool),
    /// ARGB color (e.g. `0xFF_33_AA_FF`).
    Color(u32),
    /// UTF-8 string.
    Text(String),
    /// Enum value as a 0-based index.
    EnumIndex(u32),
    /// Enum value as a label (write-only; reads come back as [`RiveValue::EnumIndex`]).
    EnumName(String),
    /// One-shot trigger (write-only).
    Trigger,
}

/// Which typed getter to use when refreshing a watched path (the property's type
/// isn't introspected for nested paths, so the caller declares it via the
/// `watch_*` methods).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatchKind {
    Number,
    Bool,
    Color,
    String,
    EnumIndex,
}

/// A view-model property that an [observed](RiveViewModel::observe) path reported
/// as **changed** — or, for a trigger, **fired** — on the last advance.
///
/// This is the modern replacement for events read-back: Rive deprecated runtime
/// *event* listening, so the rig signals gameplay by driving a view-model
/// trigger/property and the game reacts to it here. Register paths with
/// [`RiveViewModel::observe`], then consume with `MessageReader<RivePropertyChanged>`.
/// (Bevy 0.18 calls buffered events "messages".) `floor`-only for now (see the
/// module docs).
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct RivePropertyChanged {
    /// The Rive entity whose view model produced the signal.
    pub entity: Entity,
    /// The property path that changed (or the trigger path that fired).
    pub path: String,
}

/// Read/write a face's view-model properties. Spawn alongside `RiveAnimation`.
#[derive(Component, Default, Debug)]
pub struct RiveViewModel {
    /// Pending writes, drained + applied before each advance.
    writes: Vec<(String, RiveValue)>,
    /// `zero_copy` double-buffer: `writes` are moved here (main world) so the
    /// read-only extract step can ferry them to the render world, then they are
    /// cleared the following frame. Absent under `floor` (it drains `writes` inline).
    #[cfg(feature = "zero_copy")]
    staged: Vec<(String, RiveValue)>,
    /// Paths refreshed into `values` after each advance, with how to read each.
    watch: Vec<(String, WatchKind)>,
    /// Paths observed for change/fire — each emits a [`RivePropertyChanged`] on the
    /// frame its value changes or its trigger fires (`floor`-only; see module docs).
    observe: Vec<String>,
    /// Observed paths already subscribed (primed before an advance). The change
    /// flag only catches changes *after* subscription, so each path is primed once
    /// before it can fire. Internal bookkeeping; only the `floor` advance loop
    /// primes/drains, so this is `floor`-gated (dead under `zero_copy`).
    #[cfg(feature = "floor")]
    primed: std::collections::HashSet<String>,
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

    /// Queues a write to a **color** property (ARGB).
    pub fn set_color(&mut self, path: impl Into<String>, argb: u32) {
        self.writes.push((path.into(), RiveValue::Color(argb)));
    }

    /// Queues a write to a **string** property.
    pub fn set_string(&mut self, path: impl Into<String>, value: impl Into<String>) {
        self.writes.push((path.into(), RiveValue::Text(value.into())));
    }

    /// Queues a write to an **enum** property by 0-based value index.
    pub fn set_enum_index(&mut self, path: impl Into<String>, index: u32) {
        self.writes.push((path.into(), RiveValue::EnumIndex(index)));
    }

    /// Queues a write to an **enum** property by value label (name).
    pub fn set_enum_name(&mut self, path: impl Into<String>, name: impl Into<String>) {
        self.writes.push((path.into(), RiveValue::EnumName(name.into())));
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

    /// Registers a **color** property to read back each frame. Idempotent.
    pub fn watch_color(&mut self, path: impl Into<String>) {
        self.add_watch(path.into(), WatchKind::Color);
    }

    /// Registers a **string** property to read back each frame. Idempotent.
    pub fn watch_string(&mut self, path: impl Into<String>) {
        self.add_watch(path.into(), WatchKind::String);
    }

    /// Registers an **enum** property to read back (as an index) each frame. Idempotent.
    pub fn watch_enum_index(&mut self, path: impl Into<String>) {
        self.add_watch(path.into(), WatchKind::EnumIndex);
    }

    fn add_watch(&mut self, path: String, kind: WatchKind) {
        if !self.watch.iter().any(|(p, _)| *p == path) {
            self.watch.push((path, kind));
        }
    }

    /// **Observes** a property/trigger path: each frame the rig changes that
    /// property — or fires that trigger — a [`RivePropertyChanged`] message is
    /// emitted for this entity. Read them with `MessageReader<RivePropertyChanged>`.
    ///
    /// This is the modern replacement for events read-back (Rive deprecated runtime
    /// event listening; use data binding instead). Type-agnostic — observe a
    /// trigger to get a discrete signal, or a number/bool/etc. to be told when it
    /// changes (then read the new value via a matching `watch_*`). Idempotent.
    ///
    /// `floor`-only for now: under `zero_copy` the advance runs in the render world
    /// and emitting main-world events needs a back-channel (deferred, like watch
    /// read-back), so an observe there is a no-op.
    pub fn observe(&mut self, path: impl Into<String>) {
        let path = path.into();
        if !self.observe.contains(&path) {
            self.observe.push(path);
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

    /// Last read-back **color** (ARGB) for `path` (if watched as a color).
    pub fn color(&self, path: &str) -> Option<u32> {
        match self.values.get(path) {
            Some(RiveValue::Color(c)) => Some(*c),
            _ => None,
        }
    }

    /// Last read-back **string** for `path` (if watched as a string).
    pub fn text(&self, path: &str) -> Option<&str> {
        match self.values.get(path) {
            Some(RiveValue::Text(s)) => Some(s.as_str()),
            _ => None,
        }
    }

    /// Last read-back **enum index** for `path` (if watched as an enum).
    pub fn enum_index(&self, path: &str) -> Option<u32> {
        match self.values.get(path) {
            Some(RiveValue::EnumIndex(i)) => Some(*i),
            _ => None,
        }
    }

    /// `zero_copy`: true if there is staging work this frame. Cheap read (no
    /// change-detection mark), so the staging system can skip idle components.
    #[cfg(feature = "zero_copy")]
    pub(crate) fn has_staging_work(&self) -> bool {
        !self.writes.is_empty() || !self.staged.is_empty()
    }

    /// `zero_copy`: move this frame's queued writes into the staging buffer (or
    /// clear last frame's), so the read-only extract can ferry them. Called once
    /// per frame after gameplay, before extract.
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
    pub(crate) fn staged(&self) -> &[(String, RiveValue)] {
        &self.staged
    }
}

// ---- apply/read (operate on the native rive_renderer::Artboard) ----

/// Applies a slice of view-model writes to the artboard's bound view model.
/// Shared by both tiers (`floor` drains inline; `zero_copy` ferries a slice to
/// the render world). Per-write failures `warn!` and continue.
#[cfg(any(feature = "floor", feature = "zero_copy"))]
pub(crate) fn apply_writes_slice(
    artboard: &rive_renderer::Artboard,
    writes: &[(String, RiveValue)],
) {
    for (path, value) in writes {
        let res = match value {
            RiveValue::Number(n) => artboard.vm_set_number(path, *n),
            RiveValue::Bool(b) => artboard.vm_set_bool(path, *b),
            RiveValue::Color(c) => artboard.vm_set_color(path, *c),
            RiveValue::Text(s) => artboard.vm_set_string(path, s),
            RiveValue::EnumIndex(i) => artboard.vm_set_enum_index(path, *i),
            RiveValue::EnumName(n) => artboard.vm_set_enum_name(path, n),
            RiveValue::Trigger => artboard.vm_fire_trigger(path),
        };
        if let Err(e) = res {
            warn!("rive: view-model write {path:?} failed: {e}");
        }
    }
}

/// Drains queued writes to the artboard's bound view model. Call **before**
/// advancing so the state machine / scripts observe them this tick.
#[cfg(feature = "floor")]
pub(crate) fn apply_writes(vm: &mut RiveViewModel, artboard: &rive_renderer::Artboard) {
    let writes = std::mem::take(&mut vm.writes);
    apply_writes_slice(artboard, &writes);
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
            WatchKind::Color => artboard.vm_get_color(path).map(RiveValue::Color),
            WatchKind::String => artboard.vm_get_string(path).map(RiveValue::Text),
            WatchKind::EnumIndex => artboard.vm_get_enum_index(path).map(RiveValue::EnumIndex),
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

/// Primes (subscribes) any newly-[observed](RiveViewModel::observe) paths so a
/// change/fire during the *next* advance is caught. Call **before** advancing.
/// The shim's change flag only catches changes after subscription, and the first
/// `flush` reads (and discards) the initial state — so each path is primed exactly
/// once. Cheap after the first frame (only un-primed paths touch the shim).
#[cfg(feature = "floor")]
pub(crate) fn prime_observed(vm: &mut RiveViewModel, artboard: &rive_renderer::Artboard) {
    // Take `observe` out so `vm.primed` can be mutated without aliasing it.
    let observe = std::mem::take(&mut vm.observe);
    for path in &observe {
        if !vm.primed.contains(path) {
            // Subscribe + discard the initial flag (priming is not a real change).
            let _ = artboard.vm_flush_changed(path);
            vm.primed.insert(path.clone());
        }
    }
    vm.observe = observe;
}

/// Flushes [observed](RiveViewModel::observe) paths after an advance, returning
/// those that changed (or whose trigger fired) this frame — the caller emits a
/// [`RivePropertyChanged`] per returned path. Call **after** advancing.
#[cfg(feature = "floor")]
pub(crate) fn drain_observed(vm: &RiveViewModel, artboard: &rive_renderer::Artboard) -> Vec<String> {
    let mut fired = Vec::new();
    for path in &vm.observe {
        match artboard.vm_flush_changed(path) {
            Ok(true) => fired.push(path.clone()),
            Ok(false) => {}
            Err(e) => warn!("rive: view-model observe {path:?} failed: {e}"),
        }
    }
    fired
}
