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
//! ([`RiveViewModel::set_image`] — encoded bytes decoded + bound before advance),
//! **artboard-reference** writes ([`RiveViewModel::set_artboard`] / `clear_artboard`),
//! and **structural** list commands ([`RiveViewModel::list_add_new`] /
//! `list_insert_new` / `list_remove_at` / `list_swap` / `list_clear`) + VM-reference
//! replacement ([`RiveViewModel::replace_view_model`]) — construct a new instance with
//! [`NewViewModel`]. The component is tier-agnostic and works in **both tiers**:
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
    /// An **artboard-reference** binding (`propertyArtboard`, write-only): `Some(name)`
    /// binds the artboard of that name from the face's **own file**; `None` clears it.
    /// Resolved to a `BindableArtboard` at apply time (no demo asset authors this
    /// property, so it is API-verified — see `docs/feature-support.md`).
    ArtboardRef(Option<String>),
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
            Self::ArtboardRef(name) => f.debug_tuple("ArtboardRef").field(name).finish(),
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

/// How to construct a fresh view-model instance for [`NewViewModel`] — mirrors
/// rive's `ViewModelRuntime::create*` verbs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmSource {
    /// A blank instance (all default property values).
    Blank,
    /// The editor's default instance (falls back to blank if none authored).
    Default,
    /// A clone of the editor instance with this name.
    FromName(String),
    /// A clone of the editor instance at this index.
    FromIndex(usize),
}

/// A description of a **new view-model instance** to construct and add to a list
/// ([`RiveViewModel::list_add_new`] / [`RiveViewModel::list_insert_new`]) or assign to
/// a VM-reference property ([`RiveViewModel::replace_view_model`]). Names the
/// view-model definition (`vm`), how to seed it (`source`), and any initial property
/// writes (`init`, applied to the new instance before it is added — flat paths
/// relative to it). Build with [`Self::blank`] / [`Self::default_instance`] /
/// [`Self::from_name`] / [`Self::from_index`], then chain `with_*` seeds.
///
/// ```no_run
/// # use bevy_rive::prelude::*;
/// let item = NewViewModel::blank("WheelItem")
///     .with_number("value", 7.0)
///     .with_string("label", "Cherry");
/// # let _ = item;
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct NewViewModel {
    vm: String,
    source: VmSource,
    init: Vec<(String, RiveValue)>,
}

impl NewViewModel {
    /// A blank instance of view-model definition `vm`.
    pub fn blank(vm: impl Into<String>) -> Self {
        Self::new(vm, VmSource::Blank)
    }
    /// The editor's default instance of `vm` (falls back to blank).
    pub fn default_instance(vm: impl Into<String>) -> Self {
        Self::new(vm, VmSource::Default)
    }
    /// A clone of the editor instance named `instance` of definition `vm`.
    pub fn from_name(vm: impl Into<String>, instance: impl Into<String>) -> Self {
        Self::new(vm, VmSource::FromName(instance.into()))
    }
    /// A clone of the editor instance at `index` of definition `vm`.
    pub fn from_index(vm: impl Into<String>, index: usize) -> Self {
        Self::new(vm, VmSource::FromIndex(index))
    }
    fn new(vm: impl Into<String>, source: VmSource) -> Self {
        Self { vm: vm.into(), source, init: Vec::new() }
    }

    /// Seeds a **number** property on the new instance. Chainable.
    #[must_use]
    pub fn with_number(mut self, path: impl Into<String>, value: f32) -> Self {
        self.init.push((path.into(), RiveValue::Number(value)));
        self
    }
    /// Seeds a **bool** property. Chainable.
    #[must_use]
    pub fn with_bool(mut self, path: impl Into<String>, value: bool) -> Self {
        self.init.push((path.into(), RiveValue::Bool(value)));
        self
    }
    /// Seeds a **color** property (ARGB). Chainable.
    #[must_use]
    pub fn with_color(mut self, path: impl Into<String>, argb: u32) -> Self {
        self.init.push((path.into(), RiveValue::Color(argb)));
        self
    }
    /// Seeds a **string** property. Chainable.
    #[must_use]
    pub fn with_string(mut self, path: impl Into<String>, value: impl Into<String>) -> Self {
        self.init.push((path.into(), RiveValue::Text(value.into())));
        self
    }
    /// Seeds an **enum** property by 0-based value index. Chainable.
    #[must_use]
    pub fn with_enum_index(mut self, path: impl Into<String>, index: u32) -> Self {
        self.init.push((path.into(), RiveValue::EnumIndex(index)));
        self
    }
    /// Seeds an **enum** property by value label. Chainable.
    #[must_use]
    pub fn with_enum_name(mut self, path: impl Into<String>, name: impl Into<String>) -> Self {
        self.init.push((path.into(), RiveValue::EnumName(name.into())));
        self
    }
    /// Seeds an **image** property (encoded PNG/JPEG/WEBP bytes). Chainable.
    #[must_use]
    pub fn with_image(mut self, path: impl Into<String>, bytes: impl Into<Arc<[u8]>>) -> Self {
        self.init.push((path.into(), RiveValue::Image(bytes.into())));
        self
    }
    /// Seeds an **artboard-reference** property (bind the named artboard from the
    /// face's own file). Chainable.
    #[must_use]
    pub fn with_artboard(mut self, path: impl Into<String>, artboard: impl Into<String>) -> Self {
        self.init
            .push((path.into(), RiveValue::ArtboardRef(Some(artboard.into()))));
        self
    }
}

/// A queued **structural** command against a view-model list / VM-reference —
/// ferried + applied before advance like a write (both tiers), but not expressible as
/// a `(path, value)` write. `path` addresses the list (or VM-ref) — a flat/`/`-nested
/// path, or `name[i]/...` to reach through a list item (resolved via
/// [`rive_renderer::Artboard::vm_resolve`]).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ListCmd {
    /// Construct a new instance and append it (`index` = None) or insert it at `index`.
    AddNew { path: String, item: NewViewModel, index: Option<usize> },
    /// Remove the item at `index`.
    RemoveAt { path: String, index: usize },
    /// Swap the items at `a` and `b`.
    Swap { path: String, a: usize, b: usize },
    /// Remove all items.
    Clear { path: String },
    /// Construct a new instance and assign it to the VM-reference property at `path`.
    ReplaceViewModel { path: String, item: NewViewModel },
}

/// Read/write a face's view-model properties. Spawn alongside `RiveAnimation`.
#[derive(Component, Default, Debug)]
pub struct RiveViewModel {
    /// Pending writes, drained + applied before each advance.
    writes: Vec<(String, RiveValue)>,
    /// Pending **structural** commands (list add/remove/swap/clear + VM-ref replace),
    /// drained + applied before each advance, AFTER `writes` — so a value write on an
    /// existing item lands before a same-frame restructure. Populate a NEWLY
    /// constructed item via [`NewViewModel`]'s `with_*` seeds (its index isn't known
    /// until apply), not a follow-up indexed write. Ferried like `writes`.
    list_cmds: Vec<ListCmd>,
    /// `zero_copy` double-buffer: `writes` are moved here (main world) so the
    /// read-only extract step can ferry them to the render world, then they are
    /// cleared the following frame. Absent under `floor` (it drains `writes` inline).
    #[cfg(feature = "zero_copy")]
    staged: Vec<(String, RiveValue)>,
    /// `zero_copy` double-buffer for `list_cmds` (mirrors `staged`).
    #[cfg(feature = "zero_copy")]
    staged_cmds: Vec<ListCmd>,
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

    /// Queues a write to an **artboard-reference** (`propertyArtboard`) property:
    /// binds the artboard named `artboard` **from the face's own file** (so a
    /// `NestedArtboard` bound to this property instances it on the next advance). The
    /// path may descend nested view models (`/`) or index a list item (`name[i]`).
    pub fn set_artboard(&mut self, path: impl Into<String>, artboard: impl Into<String>) {
        self.writes
            .push((path.into(), RiveValue::ArtboardRef(Some(artboard.into()))));
    }

    /// Queues a clear of an **artboard-reference** property (unbinds any bound artboard).
    pub fn clear_artboard(&mut self, path: impl Into<String>) {
        self.writes.push((path.into(), RiveValue::ArtboardRef(None)));
    }

    // ---- list structural mutation + VM-instance construction ----

    /// Constructs a new view-model instance (per `item`) and **appends** it to the
    /// list property at `path`. Applied before the next advance, in both tiers. The
    /// new item's index is the list's length at apply time; to populate it, seed
    /// `item` via [`NewViewModel`]'s `with_*` (preferred — no index guessing) rather
    /// than a follow-up indexed write.
    pub fn list_add_new(&mut self, path: impl Into<String>, item: NewViewModel) {
        self.list_cmds
            .push(ListCmd::AddNew { path: path.into(), item, index: None });
    }

    /// Constructs a new view-model instance (per `item`) and **inserts** it at `index`
    /// in the list at `path` (valid range `0..=len`; `index == len` appends).
    pub fn list_insert_new(&mut self, path: impl Into<String>, index: usize, item: NewViewModel) {
        self.list_cmds
            .push(ListCmd::AddNew { path: path.into(), item, index: Some(index) });
    }

    /// Removes the item at `index` from the list at `path`.
    pub fn list_remove_at(&mut self, path: impl Into<String>, index: usize) {
        self.list_cmds
            .push(ListCmd::RemoveAt { path: path.into(), index });
    }

    /// Swaps the items at `a` and `b` in the list at `path`.
    pub fn list_swap(&mut self, path: impl Into<String>, a: usize, b: usize) {
        self.list_cmds.push(ListCmd::Swap { path: path.into(), a, b });
    }

    /// Removes all items from the list at `path`, leaving it empty.
    pub fn list_clear(&mut self, path: impl Into<String>) {
        self.list_cmds.push(ListCmd::Clear { path: path.into() });
    }

    /// Constructs a new view-model instance (per `item`) and assigns it to the
    /// **view-model-reference** property at `path`. The new instance's view-model type
    /// must match the property's referenced type.
    pub fn replace_view_model(&mut self, path: impl Into<String>, item: NewViewModel) {
        self.list_cmds
            .push(ListCmd::ReplaceViewModel { path: path.into(), item });
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
        !self.writes.is_empty()
            || !self.staged.is_empty()
            || !self.list_cmds.is_empty()
            || !self.staged_cmds.is_empty()
    }

    /// `zero_copy`: move this frame's queued writes + structural commands into their
    /// staging buffers (or clear last frame's), so the read-only extract can ferry
    /// them. Called once per frame after gameplay, before extract.
    #[cfg(feature = "zero_copy")]
    pub(crate) fn stage_writes(&mut self) {
        if self.writes.is_empty() {
            self.staged.clear();
        } else {
            self.staged = std::mem::take(&mut self.writes);
        }
        if self.list_cmds.is_empty() {
            self.staged_cmds.clear();
        } else {
            self.staged_cmds = std::mem::take(&mut self.list_cmds);
        }
    }

    /// `zero_copy`: the writes staged for this frame (ferried by extract).
    #[cfg(feature = "zero_copy")]
    pub(crate) fn staged(&self) -> &[(String, RiveValue)] {
        &self.staged
    }

    /// `zero_copy`: the structural commands staged for this frame (ferried by extract).
    #[cfg(feature = "zero_copy")]
    pub(crate) fn staged_cmds(&self) -> &[ListCmd] {
        &self.staged_cmds
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
        RiveValue::ArtboardRef(Some(name)) => {
            artboard.vm_set_artboard(path, &artboard.bindable_artboard_named(name)?)
        }
        RiveValue::ArtboardRef(None) => artboard.vm_clear_artboard(path),
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
    apply_value_to_handle(ctx, artboard, &item, &leaf, value)
}

/// Writes `value` to `leaf` on a resolved view-model-instance `handle` — the shared
/// per-property write path for indexed writes and for seeding a freshly constructed
/// instance ([`build_new_vm`]). `artboard` is needed to source a `BindableArtboard`
/// for an [`RiveValue::ArtboardRef`]; `ctx` to decode an [`RiveValue::Image`].
#[cfg(any(feature = "floor", feature = "zero_copy"))]
fn apply_value_to_handle(
    ctx: &rive_renderer::Context,
    artboard: &rive_renderer::Artboard,
    handle: &rive_renderer::RiveViewModelInstance,
    leaf: &str,
    value: &RiveValue,
) -> rive_renderer::Result<()> {
    match value {
        RiveValue::Number(n) => handle.set_number(leaf, *n),
        RiveValue::Bool(b) => handle.set_bool(leaf, *b),
        RiveValue::Color(c) => handle.set_color(leaf, *c),
        RiveValue::Text(s) => handle.set_string(leaf, s),
        RiveValue::EnumIndex(i) => handle.set_enum_index(leaf, *i),
        RiveValue::EnumName(n) => handle.set_enum_name(leaf, n),
        RiveValue::Trigger => handle.fire_trigger(leaf),
        RiveValue::Image(bytes) => handle.set_image(leaf, &ctx.decode_image(bytes)?),
        RiveValue::ArtboardRef(Some(name)) => {
            handle.set_artboard(leaf, &artboard.bindable_artboard_named(name)?)
        }
        RiveValue::ArtboardRef(None) => handle.clear_artboard(leaf),
    }
}

/// Applies a slice of structural list/VM-ref commands to the artboard's bound view
/// model. Shared by both tiers (`floor` drains inline; `zero_copy` ferries a slice).
/// Call **after** [`apply_writes_slice`] (see the `list_cmds` field doc). Per-command
/// failures `warn!` and continue.
#[cfg(any(feature = "floor", feature = "zero_copy"))]
pub(crate) fn apply_list_cmds_slice(
    ctx: &rive_renderer::Context,
    artboard: &rive_renderer::Artboard,
    cmds: &[ListCmd],
) {
    for cmd in cmds {
        if let Err(e) = apply_list_cmd(ctx, artboard, cmd) {
            warn!("rive: view-model list command {cmd:?} failed: {e}");
        }
    }
}

/// Applies one structural command. `path` addresses the list (or VM-ref property);
/// [`rive_renderer::Artboard::vm_resolve`] resolves it to the owning instance + the
/// final (list/property) name, handling flat, `/`-nested, and `name[i]/…` paths.
#[cfg(any(feature = "floor", feature = "zero_copy"))]
fn apply_list_cmd(
    ctx: &rive_renderer::Context,
    artboard: &rive_renderer::Artboard,
    cmd: &ListCmd,
) -> rive_renderer::Result<()> {
    match cmd {
        ListCmd::RemoveAt { path, index } => {
            let (owner, name) = artboard.vm_resolve(path)?;
            owner.list_remove_at(&name, *index)
        }
        ListCmd::Swap { path, a, b } => {
            let (owner, name) = artboard.vm_resolve(path)?;
            owner.list_swap(&name, *a, *b)
        }
        ListCmd::Clear { path } => {
            let (owner, name) = artboard.vm_resolve(path)?;
            owner.list_clear(&name)
        }
        ListCmd::AddNew { path, item, index } => {
            let owned = build_new_vm(ctx, artboard, item)?;
            let (owner, name) = artboard.vm_resolve(path)?;
            match index {
                Some(i) => owner.list_add_at(&name, &owned.borrow(), *i),
                None => owner.list_add(&name, &owned.borrow()),
            }
        }
        ListCmd::ReplaceViewModel { path, item } => {
            let owned = build_new_vm(ctx, artboard, item)?;
            let (owner, leaf) = artboard.vm_resolve(path)?;
            owner.replace_view_model(&leaf, &owned.borrow())
        }
    }
}

/// Constructs a fresh view-model instance per `item` (definition + source) and seeds
/// its initial properties. The caller adds it to a list / assigns it (the list/parent
/// then co-owns it). A bad seed `warn!`s and continues (one bad path doesn't abort
/// the construct).
#[cfg(any(feature = "floor", feature = "zero_copy"))]
fn build_new_vm<'a>(
    ctx: &rive_renderer::Context,
    artboard: &'a rive_renderer::Artboard,
    item: &NewViewModel,
) -> rive_renderer::Result<rive_renderer::RiveOwnedViewModel<'a>> {
    let def = artboard.view_model_by_name(&item.vm).ok_or_else(|| {
        rive_renderer::Error::ViewModel(format!("view-model definition {:?} not found", item.vm))
    })?;
    let owned = match &item.source {
        VmSource::Blank => def.create_instance()?,
        VmSource::Default => def.create_default_instance()?,
        VmSource::FromName(n) => def.create_instance_from_name(n)?,
        VmSource::FromIndex(i) => def.create_instance_from_index(*i)?,
    };
    // Borrow the new instance to seed its properties; the borrow ends with this block
    // (before `owned` is moved out — it aliases `owned`, which has no `Drop`).
    {
        let handle = owned.borrow();
        for (p, v) in &item.init {
            if let Err(e) = apply_value_to_handle(ctx, artboard, &handle, p, v) {
                warn!("rive: new view-model seed {p:?} failed: {e}");
            }
        }
    }
    Ok(owned)
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

/// Drains queued **structural** commands (list add/remove/swap/clear + VM-ref
/// replace) to the artboard's bound view model. Call **after** [`apply_writes`] and
/// **before** advancing (see the `list_cmds` field doc for the ordering rationale).
#[cfg(feature = "floor")]
pub(crate) fn apply_list_cmds(
    ctx: &rive_renderer::Context,
    vm: &mut RiveViewModel,
    artboard: &rive_renderer::Artboard,
) {
    let cmds = std::mem::take(&mut vm.list_cmds);
    apply_list_cmds_slice(ctx, artboard, &cmds);
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
