//! Runtime **text-run set** for Bevy — drive a `.riv`'s text at runtime. Attach a
//! [`RiveText`] to the same entity as [`RiveAnimation`](crate::RiveAnimation) and
//! queue set-writes; each is applied to the matching `TextValueRun` before the
//! next advance (so the state machine / scripts see the new text this tick), in
//! BOTH tiers (`floor` inline; `zero_copy` ferried to the render world, like
//! view-model writes).
//!
//! Reads (a run's current string) are available at the safe layer
//! ([`Artboard::text_get`](rive_renderer::Artboard::text_get)); a Bevy read-back
//! channel is deferred (see `docs/feature-support.md`), mirroring the view-model
//! watch read-back.

use bevy::prelude::*;

/// One queued text-run set: the run `name`, an optional nested-artboard `path`
/// (empty = the top-level artboard), and the new `value`.
#[derive(Clone, Debug)]
pub(crate) struct TextWrite {
    pub path: String,
    pub name: String,
    pub value: String,
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
