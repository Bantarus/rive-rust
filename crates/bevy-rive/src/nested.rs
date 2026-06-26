//! Redirect an entity's **rig / text** writes to a NESTED child artboard.
//!
//! A `.riv` artboard can mount other artboards via `NestedArtboard` components.
//! Attaching a [`RiveNestedTarget`] alongside [`RiveRig`](crate::RiveRig) /
//! [`RiveText`](crate::RiveText) on an entity redirects those writes to the
//! addressed child instead of the root artboard — the child is auto-advanced by the
//! root, so the usual assert-before-advance contract still holds. Honored in both
//! tiers (`floor` inline; `zero_copy` ferried to the render world).
//!
//! Only the pure artboard-component writes (bones / constraints / solos / text runs)
//! are redirected; pointer + keyboard/gamepad/focus input stay on the root state
//! machine (a nested child has no separate scene). Drive several children from
//! several entities, or address them at the safe layer
//! ([`Artboard::nested_artboard`](rive_renderer::Artboard::nested_artboard)).

use bevy::prelude::*;
use rive_renderer::Artboard;

/// Redirects the [`RiveRig`](crate::RiveRig) / [`RiveText`](crate::RiveText) writes
/// on this entity to a **nested child** artboard (resolved against the entity's root
/// artboard each frame). Without it, those writes target the root.
///
/// Use [`RiveNestedTarget::path`] for a `/`-delimited `NestedArtboard` path, or
/// [`RiveNestedTarget::index`] when the components are unnamed (the common case —
/// designers often leave `NestedArtboard` instances unnamed). A target that does not
/// resolve `warn!`s and falls back to the root (so a typo never panics).
#[derive(Component, Clone, Debug, PartialEq, Eq)]
pub enum RiveNestedTarget {
    /// Resolve by a `/`-delimited `NestedArtboard` path ("child/grandchild").
    Path(String),
    /// Resolve by 0-based index in nested order (for unnamed components).
    Index(usize),
}

impl RiveNestedTarget {
    /// Targets a nested child by `/`-delimited component path.
    pub fn path(p: impl Into<String>) -> Self {
        Self::Path(p.into())
    }

    /// Targets a nested child by 0-based index (for unnamed `NestedArtboard`s).
    pub fn index(i: usize) -> Self {
        Self::Index(i)
    }

    /// Resolves to the child [`Artboard`], or `None` (after a `warn!`) if it does not
    /// resolve — the caller then falls back to the root.
    #[cfg(any(feature = "floor", feature = "zero_copy"))]
    pub(crate) fn resolve(&self, root: &Artboard) -> Option<Artboard> {
        let resolved = match self {
            Self::Path(p) => root.nested_artboard_at_path(p),
            Self::Index(i) => root.nested_artboard_at(*i),
        };
        match resolved {
            Ok(child) => Some(child),
            Err(e) => {
                warn!("rive: nested target {self:?} did not resolve (using root): {e}");
                None
            }
        }
    }
}
