//! Out-of-band asset loading for Bevy — supplying the images / fonts / audio a
//! `.riv` references externally (assets exported as **Referenced**, not
//! **Embedded**). Attach a [`RiveAssets`] to the same entity as
//! [`RiveAnimation`](crate::RiveAnimation); each referenced asset is resolved
//! from the map by its authored name when the `.riv` is instantiated (once).
//! Honored in **both tiers** (the `zero_copy` tier ferries the map — a cheap
//! [`Arc`] refcount bump — to the render world where its instances are built).
//!
//! ```no_run
//! use bevy_rive::RiveAssets;
//! let assets = RiveAssets::new()
//!     .with("logo.png", std::fs::read("logo.png").unwrap())
//!     .with("Inter.ttf", std::fs::read("Inter.ttf").unwrap());
//! ```
//!
//! Supply **encoded** file bytes (a PNG / JPEG / WEBP image, or a font / audio
//! file); rive decodes them via the render context. An asset whose name is not
//! in the map falls back to the file's in-band content (if any).

use std::collections::HashMap;
use std::sync::Arc;

use bevy::prelude::*;

/// Encoded asset bytes keyed by authored name. Lives behind the [`RiveAssets`]
/// [`Arc`] so the component clones cheaply.
#[derive(Default, Debug, Clone)]
pub(crate) struct AssetMap {
    by_name: HashMap<String, Vec<u8>>,
}

/// A map of out-of-band assets (images / fonts / audio) supplied to a `.riv` by
/// authored name. Attach to the same entity as
/// [`RiveAnimation`](crate::RiveAnimation); referenced assets are resolved once,
/// when the file is instantiated. Cloning is cheap (the map lives behind an
/// [`Arc`]), so the `zero_copy` tier ferries it to the render world for free.
/// Honored in both tiers.
#[derive(Component, Default, Clone, Debug)]
pub struct RiveAssets {
    map: Arc<AssetMap>,
}

impl RiveAssets {
    /// An empty map.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts (or replaces) the **encoded** bytes for the asset with this
    /// authored `name` (e.g. `"logo.png"`) — a PNG / JPEG / WEBP image, or a
    /// font / audio file's bytes.
    pub fn insert(&mut self, name: impl Into<String>, bytes: impl Into<Vec<u8>>) -> &mut Self {
        Arc::make_mut(&mut self.map)
            .by_name
            .insert(name.into(), bytes.into());
        self
    }

    /// Builder form of [`insert`](Self::insert).
    #[must_use]
    pub fn with(mut self, name: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Self {
        self.insert(name, bytes);
        self
    }

    /// The encoded bytes registered for `name`, if any.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&[u8]> {
        self.map.by_name.get(name).map(Vec::as_slice)
    }

    /// Whether the map has no assets.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.by_name.is_empty()
    }

    /// The number of assets registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.by_name.len()
    }
}

/// Loads `bytes` through `ctx`, resolving referenced assets from `assets` (by
/// authored name). When `assets` is `None` or empty this is exactly
/// [`Context::load_file`](rive_renderer::Context::load_file), keeping the common
/// no-assets path unchanged. Shared by both tiers' instantiation paths.
#[cfg(any(feature = "floor", feature = "zero_copy"))]
pub(crate) fn load_file_with_assets(
    ctx: &rive_renderer::Context,
    bytes: &[u8],
    assets: Option<&RiveAssets>,
) -> rive_renderer::Result<rive_renderer::File> {
    match assets {
        Some(a) if !a.is_empty() => {
            ctx.load_file_with_assets(bytes, |req| a.get(req.name).map(<[u8]>::to_vec))
        }
        _ => ctx.load_file(bytes),
    }
}
