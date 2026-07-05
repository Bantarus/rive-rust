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
//! Property paths use `/` to descend nested view models (e.g. `"breath/scaleX"`)
//! and `name[i]` to index a **list** element (e.g. `"wheels[2]/value"`) — so a
//! write can reach into a list item, which a flat path can't (rive's resolver
//! can't index lists). Indexing works for any `set_*`/`fire_trigger` here, in both
//! tiers; watch/observe stay on flat paths for now. Mirrors the C++ contract in the
//! Rive data-binding docs (https://rive.app/docs).
//!
//! Covers number / bool / trigger / color / string / enum, plus **image** writes
//! ([`RiveViewModel::set_image`] — encoded bytes decoded + bound before advance).
//! The component is tier-agnostic and works in **both tiers**:
//! - **Writes**: the `floor` advance system applies them inline; the `zero_copy`
//!   tier ferries them to the render world (where its instances live) via a
//!   per-frame staging buffer.
//! - **Watch read-back** AND **observe** (change/trigger fires): `floor` reads
//!   inline after advance (same frame); `zero_copy` reads after the render-node
//!   advance and ships the results back over the **render→main back-channel**
//!   (`RiveReadbackChannel` in `zero_copy.rs`), drained into
//!   [`RiveViewModel::values`] / [`RivePropertyChanged`] in `PreUpdate` — so
//!   under `zero_copy` a read-back lands one frame after the advance it observed
//!   (the node runs after the main schedule).

use std::collections::HashMap;
use std::sync::Arc;

use bevy::prelude::*;

/// A typed view-model value — produced by writes and stored in read-back.
///
/// `Debug` is hand-written so [`RiveValue::Image`] prints its byte length rather
/// than dumping the whole (potentially multi-MB) buffer — the enclosing
/// [`RiveViewModel`] is a `Debug` component that an inspector may print.
#[derive(Clone, PartialEq)]
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
    /// Encoded image bytes (PNG / JPEG / WEBP) to bind to an image property
    /// (write-only). Decoded at apply time via the owning context. `Arc` so the
    /// `zero_copy` ferry to the render world is a cheap refcount bump.
    Image(Arc<[u8]>),
}

impl std::fmt::Debug for RiveValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Number(n) => f.debug_tuple("Number").field(n).finish(),
            Self::Bool(b) => f.debug_tuple("Bool").field(b).finish(),
            Self::Color(c) => write!(f, "Color({c:#010X})"),
            Self::Text(s) => f.debug_tuple("Text").field(s).finish(),
            Self::EnumIndex(i) => f.debug_tuple("EnumIndex").field(i).finish(),
            Self::EnumName(n) => f.debug_tuple("EnumName").field(n).finish(),
            Self::Trigger => f.write_str("Trigger"),
            // Don't dump the buffer — just its size (see the type doc).
            Self::Image(bytes) => write!(f, "Image({} bytes)", bytes.len()),
        }
    }
}

/// Which typed getter to use when refreshing a watched path (the property's type
/// isn't introspected for nested paths, so the caller declares it via the
/// `watch_*` methods). `pub(crate)`: the `zero_copy` extract ferries the watch
/// list to the render world, where [`read_watch_slice`] uses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatchKind {
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
/// (Bevy 0.18 calls buffered events "messages".) Both tiers: `floor` emits during
/// its advance system (same frame); `zero_copy` emits from the `PreUpdate` drain of
/// the render→main back-channel (one frame after the advance that fired — see the
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
    /// frame its value changes or its trigger fires (both tiers; see module docs).
    observe: Vec<String>,
    /// Observed paths already subscribed (primed before an advance). The change
    /// flag only catches changes *after* subscription, so each path is primed once
    /// before it can fire. Internal bookkeeping; only the `floor` advance loop
    /// primes/drains THIS set, so it is `floor`-gated — the `zero_copy` tier keeps
    /// its primed set on the render-world instance instead (the instance owns the
    /// artboard the subscription lives on, so an instance rebuild re-primes).
    #[cfg(feature = "floor")]
    primed: std::collections::HashSet<String>,
    /// Latest read-back of each watched path (refreshed after each advance; under
    /// `zero_copy` it lands one frame later and a culled face keeps its last
    /// read-back — see the module docs).
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

    /// Queues a write to an **image** property: `bytes` are encoded image data
    /// (PNG / JPEG / WEBP), decoded via the owning context before the next advance
    /// and bound to the property. Accepts a `Vec<u8>`, `&[u8]`, or `Arc<[u8]>`
    /// (the bytes are held in an `Arc` for the cheap `zero_copy` ferry). The path
    /// may descend nested view models (`/`) or index a list item (`name[i]`).
    pub fn set_image(&mut self, path: impl Into<String>, bytes: impl Into<Arc<[u8]>>) {
        self.writes.push((path.into(), RiveValue::Image(bytes.into())));
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
    /// Works in both tiers. Under `zero_copy` the advance runs in the render world,
    /// so fires travel back over the render→main back-channel and the message is
    /// emitted one frame after the advance that fired (see the module docs).
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

    /// `zero_copy`: the registered watch list (path + declared type), ferried by
    /// extract to the render world where the node reads it after advance.
    #[cfg(feature = "zero_copy")]
    pub(crate) fn watch_list(&self) -> &[(String, WatchKind)] {
        &self.watch
    }

    /// `zero_copy`: the registered observe list, ferried by extract to the render
    /// world where the node primes (before advance) + flushes (after advance) it.
    #[cfg(feature = "zero_copy")]
    pub(crate) fn observe_list(&self) -> &[String] {
        &self.observe
    }
}

// ---- apply/read (operate on the native rive_renderer::Artboard) ----

/// Applies a slice of view-model writes to the artboard's bound view model.
/// Shared by both tiers (`floor` drains inline; `zero_copy` ferries a slice to
/// the render world). `ctx` is the artboard's owning context — needed to decode
/// image writes (it must be the same context the artboard renders with). Per-write
/// failures `warn!` and continue.
#[cfg(any(feature = "floor", feature = "zero_copy"))]
pub(crate) fn apply_writes_slice(
    ctx: &rive_renderer::Context,
    artboard: &rive_renderer::Artboard,
    writes: &[(String, RiveValue)],
) {
    for (path, value) in writes {
        // A `[` means an indexed (list-item) path the flat resolver can't address;
        // route it through `vm_resolve`. Plain paths keep the proven flat setters.
        let res = if path.contains('[') {
            apply_indexed_write(ctx, artboard, path, value)
        } else {
            apply_flat_write(ctx, artboard, path, value)
        };
        if let Err(e) = res {
            warn!("rive: view-model write {path:?} failed: {e}");
        }
    }
}

/// Writes `value` to a flat root-VM path (`/` descends named nested view models).
#[cfg(any(feature = "floor", feature = "zero_copy"))]
fn apply_flat_write(
    ctx: &rive_renderer::Context,
    artboard: &rive_renderer::Artboard,
    path: &str,
    value: &RiveValue,
) -> rive_renderer::Result<()> {
    match value {
        RiveValue::Number(n) => artboard.vm_set_number(path, *n),
        RiveValue::Bool(b) => artboard.vm_set_bool(path, *b),
        RiveValue::Color(c) => artboard.vm_set_color(path, *c),
        RiveValue::Text(s) => artboard.vm_set_string(path, s),
        RiveValue::EnumIndex(i) => artboard.vm_set_enum_index(path, *i),
        RiveValue::EnumName(n) => artboard.vm_set_enum_name(path, n),
        RiveValue::Trigger => artboard.vm_fire_trigger(path),
        RiveValue::Image(bytes) => artboard.vm_set_image(path, &ctx.decode_image(bytes)?),
    }
}

/// Writes `value` to an indexed path (e.g. `"wheels[2]/value"`): resolves the
/// owning instance + leaf via [`rive_renderer::Artboard::vm_resolve`], then writes
/// into that nested view model / list item.
#[cfg(any(feature = "floor", feature = "zero_copy"))]
fn apply_indexed_write(
    ctx: &rive_renderer::Context,
    artboard: &rive_renderer::Artboard,
    path: &str,
    value: &RiveValue,
) -> rive_renderer::Result<()> {
    let (item, leaf) = artboard.vm_resolve(path)?;
    match value {
        RiveValue::Number(n) => item.set_number(&leaf, *n),
        RiveValue::Bool(b) => item.set_bool(&leaf, *b),
        RiveValue::Color(c) => item.set_color(&leaf, *c),
        RiveValue::Text(s) => item.set_string(&leaf, s),
        RiveValue::EnumIndex(i) => item.set_enum_index(&leaf, *i),
        RiveValue::EnumName(n) => item.set_enum_name(&leaf, n),
        RiveValue::Trigger => item.fire_trigger(&leaf),
        RiveValue::Image(bytes) => item.set_image(&leaf, &ctx.decode_image(bytes)?),
    }
}

/// Drains queued writes to the artboard's bound view model. Call **before**
/// advancing so the state machine / scripts observe them this tick. `ctx` is the
/// artboard's owning context (used to decode image writes).
#[cfg(feature = "floor")]
pub(crate) fn apply_writes(
    ctx: &rive_renderer::Context,
    vm: &mut RiveViewModel,
    artboard: &rive_renderer::Artboard,
) {
    let writes = std::mem::take(&mut vm.writes);
    apply_writes_slice(ctx, artboard, &writes);
}

/// Reads each watched path with its declared typed getter, returning the
/// successful `(path, value)` pairs. Shared by both tiers (`floor` writes them
/// into [`RiveViewModel::values`] inline; `zero_copy` ships them back over the
/// render→main channel). Call **after** advancing so reads reflect this tick's
/// script / state-machine output. Per-path failures `warn!` and continue.
#[cfg(any(feature = "floor", feature = "zero_copy"))]
pub(crate) fn read_watch_slice(
    artboard: &rive_renderer::Artboard,
    watch: &[(String, WatchKind)],
) -> Vec<(String, RiveValue)> {
    let mut values = Vec::new();
    for (path, kind) in watch {
        let read = match kind {
            WatchKind::Number => artboard.vm_get_number(path).map(RiveValue::Number),
            WatchKind::Bool => artboard.vm_get_bool(path).map(RiveValue::Bool),
            WatchKind::Color => artboard.vm_get_color(path).map(RiveValue::Color),
            WatchKind::String => artboard.vm_get_string(path).map(RiveValue::Text),
            WatchKind::EnumIndex => artboard.vm_get_enum_index(path).map(RiveValue::EnumIndex),
        };
        match read {
            Ok(v) => values.push((path.clone(), v)),
            Err(e) => warn!("rive: view-model read {path:?} failed: {e}"),
        }
    }
    values
}

/// Primes (subscribes) any not-yet-primed observed path so a change/fire during
/// the *next* advance is caught. Call **before** advancing. The shim's change
/// flag only catches changes after subscription, and the first `flush` reads
/// (and discards) the initial state — so each path is primed exactly once per
/// `primed` set. Shared by both tiers (`floor` keeps `primed` on the component;
/// `zero_copy` on the render-world instance, whose artboard owns the
/// subscription). Cheap after the first frame (only un-primed paths touch the shim).
#[cfg(any(feature = "floor", feature = "zero_copy"))]
pub(crate) fn prime_observed_slice(
    artboard: &rive_renderer::Artboard,
    observe: &[String],
    primed: &mut std::collections::HashSet<String>,
) {
    // Drop primed paths no longer observed, so a later RE-observe re-primes (and
    // its priming flush discards state accumulated while unobserved) instead of
    // delivering a stale fire. Matters for `zero_copy`, whose primed set lives on
    // the render-world instance and so survives a component remove + re-insert
    // (floor's dies with the component; there `primed ⊆ observe` always holds and
    // this retain is a no-op).
    primed.retain(|p| observe.contains(p));
    for path in observe {
        if !primed.contains(path) {
            // Subscribe + discard the initial flag (priming is not a real change).
            let _ = artboard.vm_flush_changed(path);
            primed.insert(path.clone());
        }
    }
}

/// Flushes each observed path after an advance, returning those that changed (or
/// whose trigger fired) this frame — the caller emits a [`RivePropertyChanged`]
/// per returned path. Call **after** advancing. Shared by both tiers.
#[cfg(any(feature = "floor", feature = "zero_copy"))]
pub(crate) fn drain_observed_slice(
    artboard: &rive_renderer::Artboard,
    observe: &[String],
) -> Vec<String> {
    let mut fired = Vec::new();
    for path in observe {
        match artboard.vm_flush_changed(path) {
            Ok(true) => fired.push(path.clone()),
            Ok(false) => {}
            Err(e) => warn!("rive: view-model observe {path:?} failed: {e}"),
        }
    }
    fired
}

/// Refreshes watched paths into [`RiveViewModel::values`]. Call **after**
/// advancing so reads reflect this tick's script / state-machine output.
#[cfg(feature = "floor")]
pub(crate) fn refresh_watch(vm: &mut RiveViewModel, artboard: &rive_renderer::Artboard) {
    if vm.watch.is_empty() {
        return;
    }
    for (path, value) in read_watch_slice(artboard, &vm.watch) {
        vm.values.insert(path, value);
    }
}

/// Primes (subscribes) any newly-[observed](RiveViewModel::observe) paths so a
/// change/fire during the *next* advance is caught. Call **before** advancing.
/// (The `floor` wrapper over [`prime_observed_slice`], using the component's
/// `primed` set.)
#[cfg(feature = "floor")]
pub(crate) fn prime_observed(vm: &mut RiveViewModel, artboard: &rive_renderer::Artboard) {
    // Take `observe` out so `vm.primed` can be mutated without aliasing it.
    let observe = std::mem::take(&mut vm.observe);
    prime_observed_slice(artboard, &observe, &mut vm.primed);
    vm.observe = observe;
}

/// Flushes [observed](RiveViewModel::observe) paths after an advance, returning
/// those that changed (or whose trigger fired) this frame — the caller emits a
/// [`RivePropertyChanged`] per returned path. Call **after** advancing. (The
/// `floor` wrapper over [`drain_observed_slice`].)
#[cfg(feature = "floor")]
pub(crate) fn drain_observed(vm: &RiveViewModel, artboard: &rive_renderer::Artboard) -> Vec<String> {
    drain_observed_slice(artboard, &vm.observe)
}
