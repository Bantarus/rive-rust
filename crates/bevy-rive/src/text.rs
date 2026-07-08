//! Runtime **text-run set** for Bevy — drive a `.riv`'s text at runtime. Attach a
//! [`RiveText`] to the same entity as [`RiveAnimation`](crate::RiveAnimation) and
//! queue set-writes; each is applied to the matching `TextValueRun` before the
//! next advance (so the state machine / scripts see the new text this tick), in
//! BOTH tiers (`floor` inline; `zero_copy` ferried to the render world, like
//! view-model writes).
//!
//! Reads (a run's current string) ride the same **register-then-read-back** model
//! as the rig's `watch_*`: call [`RiveText::watch_text`] /
//! [`watch_text_in`](RiveText::watch_text_in), then read the last value with
//! [`text`](RiveText::text) / [`text_in`](RiveText::text_in). Refreshed after each
//! advance in BOTH tiers — `floor` inline (same frame), `zero_copy` over the
//! render→main back-channel (`RiveReadbackChannel`; one frame of latency, like the
//! rig / view-model watch read-back).

use std::collections::HashMap;

use bevy::prelude::*;

/// One queued text-run set: the run `name`, an optional nested-artboard `path`
/// (empty = the top-level artboard), and the new `value`.
#[derive(Clone, Debug)]
pub(crate) struct TextWrite {
    pub path: String,
    pub name: String,
    pub value: String,
}

/// One registered text-run **read** (by nested `path` + run `name`), refreshed into
/// [`RiveText`]'s read-back store after each advance (the read analogue of
/// [`TextWrite`]). `PartialEq`/`Eq` so the drain can confirm a read is still
/// registered before delivering its value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TextRead {
    pub path: String,
    pub name: String,
}

/// The read-back store key for a `(path, name)` text-run read. Length-prefixing the
/// path keeps the key injective: both are arbitrary authored strings, so a bare
/// separator could let one `(path, name)` alias a different `(path', name')` pair.
fn text_key(path: &str, name: &str) -> String {
    format!("{}:{path}{name}", path.len())
}

/// Queues runtime text-run **set** writes for a `.riv` instance. Attach to the
/// same entity as [`RiveAnimation`](crate::RiveAnimation); each queued write sets
/// a `TextValueRun`'s string before the next advance. Honored in both tiers.
#[derive(Component, Default, Debug)]
pub struct RiveText {
    /// Pending writes, drained + applied before each advance.
    writes: Vec<TextWrite>,
    /// `zero_copy` double-buffer: `writes` are moved here (main world) so the
    /// read-only extract can ferry them to the render world, then cleared the
    /// following frame. Absent under `floor` (it drains `writes` inline).
    #[cfg(feature = "zero_copy")]
    staged: Vec<TextWrite>,
    /// Registered reads (by nested `path` + run `name`), refreshed into `values`
    /// after each advance. A persistent registration (not drained) — ferried each
    /// frame to the render world under `zero_copy`, like the rig read list.
    reads: Vec<TextRead>,
    /// Latest read-back string of each registered read, keyed by [`text_key`].
    /// Written after advance (`floor` inline; `zero_copy` from the drain); read by
    /// [`text`](Self::text) / [`text_in`](Self::text_in). A culled face keeps its
    /// last read-back (no advance ⇒ no refresh).
    values: HashMap<String, String>,
}

impl RiveText {
    /// Queues a set on the **top-level** artboard's text run named `name`.
    pub fn set(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.writes.push(TextWrite {
            path: String::new(),
            name: name.into(),
            value: value.into(),
        });
    }

    /// Queues a set on a text run inside the nested artboard at `path` (a
    /// `/`-style path; empty selects the top-level artboard).
    pub fn set_in(
        &mut self,
        path: impl Into<String>,
        name: impl Into<String>,
        value: impl Into<String>,
    ) {
        self.writes.push(TextWrite {
            path: path.into(),
            name: name.into(),
            value: value.into(),
        });
    }

    /// Registers the **top-level** text run named `name` to read its live string
    /// back into this component after each advance. Read it with [`text`](Self::text).
    /// Idempotent.
    pub fn watch_text(&mut self, name: impl Into<String>) {
        self.add_read(TextRead {
            path: String::new(),
            name: name.into(),
        });
    }

    /// Registers a text run inside the nested artboard at `path` (a `/`-style path;
    /// empty selects the top-level artboard) to read back each advance. Read it with
    /// [`text_in`](Self::text_in). Idempotent.
    pub fn watch_text_in(&mut self, path: impl Into<String>, name: impl Into<String>) {
        self.add_read(TextRead {
            path: path.into(),
            name: name.into(),
        });
    }

    fn add_read(&mut self, read: TextRead) {
        if !self.reads.contains(&read) {
            self.reads.push(read);
        }
    }

    /// Last read-back string of the **top-level** text run named `name` (if
    /// [watched](Self::watch_text)). Reflects the run *after* the last advance (one
    /// frame late under `zero_copy`). `None` until the first read-back lands; a read
    /// that later starts failing warns and keeps the last successful value.
    pub fn text(&self, name: &str) -> Option<&str> {
        self.text_in("", name)
    }

    /// Last read-back string of the text run named `name` inside the nested artboard
    /// at `path` (empty selects the top-level artboard; if
    /// [watched](Self::watch_text_in)).
    pub fn text_in(&self, path: &str, name: &str) -> Option<&str> {
        self.values.get(&text_key(path, name)).map(String::as_str)
    }

    /// Whether any text read is registered — gates the `floor` advance loop's
    /// post-advance nested re-resolve + refresh (checked through `Deref`, so an
    /// unwatched face doesn't trip change detection).
    #[cfg(feature = "floor")]
    pub(crate) fn has_reads(&self) -> bool {
        !self.reads.is_empty()
    }

    /// `zero_copy`: the registered read list, ferried by extract to the render world
    /// where the node reads it after advance and ships results back over the channel.
    #[cfg(feature = "zero_copy")]
    pub(crate) fn read_list(&self) -> &[TextRead] {
        &self.reads
    }

    /// `zero_copy`: store one read-back delivered by the drain, but only if `read` is
    /// still registered — the value was produced from LAST frame's reads and the
    /// component may have dropped it since (mirrors the rig drain's guard).
    #[cfg(feature = "zero_copy")]
    pub(crate) fn store_read(&mut self, read: &TextRead, value: String) {
        if self.reads.contains(read) {
            self.values.insert(text_key(&read.path, &read.name), value);
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
    pub(crate) fn staged(&self) -> &[TextWrite] {
        &self.staged
    }
}

/// Applies a slice of text-run set writes to the artboard. Shared by both tiers
/// (`floor` drains inline; `zero_copy` ferries a slice to the render world).
/// Per-write failures `warn!` and continue.
#[cfg(any(feature = "floor", feature = "zero_copy"))]
pub(crate) fn apply_text_writes_slice(artboard: &rive_renderer::Artboard, writes: &[TextWrite]) {
    for w in writes {
        if let Err(e) = artboard.text_set_in(&w.name, &w.path, &w.value) {
            warn!(
                "rive: text run set {:?} (path {:?}) failed: {e}",
                w.name, w.path
            );
        }
    }
}

/// Drains queued text writes to the artboard. Call **before** advancing so the
/// new text is shaped + visible this tick.
#[cfg(feature = "floor")]
pub(crate) fn apply_text_writes(text: &mut RiveText, artboard: &rive_renderer::Artboard) {
    let writes = std::mem::take(&mut text.writes);
    apply_text_writes_slice(artboard, &writes);
}

/// Reads each registered text run, returning the successful `(read, value)` pairs.
/// Shared by both tiers (`floor` stores them into [`RiveText`] inline via
/// [`refresh_text_reads`]; `zero_copy` ships them back over the render→main channel).
/// Call **after** advancing so reads reflect this tick's shaped text. Per-read
/// failures `warn!` and continue — NOTE a read is a persistent registration, so a bad
/// name/path warns every frame (deliberately matching the rig `read_rig_slice`
/// convention; fix the registration, don't mute the log).
#[cfg(any(feature = "floor", feature = "zero_copy"))]
pub(crate) fn read_text_slice(
    artboard: &rive_renderer::Artboard,
    reads: &[TextRead],
) -> Vec<(TextRead, String)> {
    let mut out = Vec::new();
    for read in reads {
        match artboard.text_get_in(&read.name, &read.path) {
            Ok(v) => out.push((read.clone(), v)),
            Err(e) => warn!(
                "rive: text read {:?} (path {:?}) failed: {e}",
                read.name, read.path
            ),
        }
    }
    out
}

/// Refreshes registered text reads into [`RiveText`]'s read-back store. Call **after**
/// advancing so reads reflect this tick's shaped text. (The `floor` wrapper over
/// [`read_text_slice`], writing the component's `values` inline.)
#[cfg(feature = "floor")]
pub(crate) fn refresh_text_reads(text: &mut RiveText, artboard: &rive_renderer::Artboard) {
    if text.reads.is_empty() {
        return;
    }
    // Take `reads` out so `values` can be mutated without aliasing it.
    let reads = std::mem::take(&mut text.reads);
    for (read, value) in read_text_slice(artboard, &reads) {
        text.values.insert(text_key(&read.path, &read.name), value);
    }
    text.reads = reads;
}
