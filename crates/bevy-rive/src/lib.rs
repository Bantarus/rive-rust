//! `bevy-rive` — a Bevy plugin that drives the **native Rive Renderer** to fill a
//! Bevy [`Image`] every frame; display it with a `Sprite`.
//!
//! # Consuming `bevy-rive` (tiers & setup)
//!
//! The whole public surface is [`prelude`]. Two render tiers, selected by feature:
//!
//! - **`floor`** (default) — the CPU-copy bridge: a near-drop-in plugin with NO
//!   `ash`, NO `wgpu-hal`, NO exact-wgpu pin and NO `as_hal` code, depending only on a
//!   caret `bevy = "0.18"`. The standard three-step third-party flow:
//!   ```no_run
//!   use bevy::prelude::*;
//!   use bevy_rive::prelude::*;
//!   # let _ = |app: &mut App, handle: Handle<RiveFile>, commands: &mut Commands| {
//!   app.add_plugins(RivePlugin);                       // 1. add the plugin
//!   commands.spawn((RiveAnimation::new(handle),        // 2. spawn the components
//!                   RiveTarget::new(512, 512)));
//!   // 3. once the plugin writes `target.image` back, display it with a `Sprite`.
//!   # };
//!   ```
//!
//! - **`zero_copy`** (opt-in; **version-LOCKED**) — the Vulkan fast path (a shared
//!   `VkImage`, no per-frame readback). It is ABI-locked to **Bevy 0.18.1 / wgpu 27.0.1
//!   / wgpu-hal 27.0.4 / ash 0.38** by exact Cargo pins, so a mismatched host wgpu is a
//!   *resolver error naming the versions*, never a silent `as_hal` corruption (see
//!   `build.rs` + [`RiveZeroCopyPlugin`]). Setup is heavier and host-coupled:
//!   1. `bevy_rive::install_interlock_device_callback(&mut app)` **before** `DefaultPlugins`;
//!   2. `add_plugins(RiveZeroCopyPlugin::default())` instead of `RivePlugin` — a pure-3D
//!      scene (`Camera3d`, no `Camera2d`) needs `RiveZeroCopyPlugin::anchored(RiveGraphAnchor::Core3d)`,
//!      since the fill node only runs in a sub-graph a camera targets (default is `Both`);
//!   3. build `DefaultPlugins.disable::<PipelinedRenderingPlugin>()`;
//!   4. run on the Vulkan backend (`WGPU_BACKEND=vulkan`, until the D3D12 port).
//!
//!   Treat every Bevy bump as a deliberate zero-copy re-validation, never a `cargo update`.
//!
//! # Milestone 1a — the CPU-copy bridge (the universal fallback tier)
//!
//! The native renderer renders a `.riv`'s state machine to its own offscreen
//! Vulkan texture; this plugin reads the pixels back to the CPU and copies them
//! into a Bevy [`Image`]. No GPU sharing, no `wgpu-hal`, no render-graph node —
//! that is M1b (zero-copy).
//!
//! ## What is frozen
//!
//! The **public type surface** is the long-lived contract that later tiers reuse
//! verbatim: the [`RiveFile`] asset + its loader, the [`RiveAnimation`] /
//! [`RiveTarget`] components (and the [`ArtboardSelector`] /
//! [`StateMachineSelector`] enums), and the **orientation convention** of the
//! texture behind [`RiveTarget::image`].
//!
//! Deliberately **not** frozen (M1a implementation detail): the *systems* and the
//! way the [`Image`] is filled (CPU readback + copy, `MAIN_WORLD | RENDER_WORLD`
//! residency, the `Assets::get_mut` re-upload). M1b replaces these with a
//! `RENDER_WORLD`-only shared `VkImage` (`data: None`, no CPU copy). Also not
//! frozen: the *internal* texture format + fill mechanism (M1a CPU-readback; M1b a
//! shared `Rgba8Unorm` + a GPU un-premultiply pass). `RiveTarget.image` itself is
//! **straight-alpha `Rgba8UnormSrgb` on both tiers** — read the format off the
//! `Image`, not from a constant.
//!
//! ```no_run
//! use bevy::prelude::*;
//! use bevy_rive::{RiveAnimation, RivePlugin, RiveTarget};
//!
//! App::new()
//!     .add_plugins(DefaultPlugins)   // must precede RivePlugin (Core2dPlugin etc.)
//!     .add_plugins(RivePlugin)
//!     .run();
//! // ...then spawn `(RiveAnimation::new(handle), RiveTarget::new(512, 512))`, and
//! // once the plugin writes the image handle back, display it with
//! // `Sprite::from_image(target.image.clone())`.
//! ```
//!
//! # Threading
//!
//! The native handles are `!Send + !Sync`, so they live in `NonSend` resources
//! and every plugin system is pinned to the main thread.
//!
//! # Display, color & orientation contract
//!
//! Display the [`RiveTarget::image`] with a Bevy **`Sprite`**. A `Sprite` re-binds
//! its texture on `AssetEvent::Modified`, so it tracks the per-frame re-uploaded
//! image. A custom `Material2d` (or a 3D `StandardMaterial`) **caches its bind
//! group** and would freeze on the first frame under M1a's per-frame GPU-texture
//! recreation — those become viable in M1b, whose shared texture is stable.
//!
//! The image is **straight-alpha**, sRGB-encoded `RGBA8`, top-down rows (upright
//! by construction — the shim is the only place orientation is corrected),
//! allocated as [`TextureFormat::Rgba8UnormSrgb`]. rive's native output is
//! premultiplied; **both tiers un-premultiply**, so a straight-alpha `Sprite` (or a
//! 3D `StandardMaterial` with `AlphaMode::Blend`) composites correctly in linear
//! space for **both** opaque (matching the M0/M1.0 references exactly) and
//! transparent content. M1a un-premultiplies on CPU readback; M1b's zero-copy path
//! does it in a fullscreen GPU pass (un-premultiply + sRGB-decode, design spec §7
//! Option B). The straight-alpha convention is therefore **uniform across tiers** —
//! see `docs/M1A_REPORT.md` and `docs/design/M1B_DESIGN_SPEC.md` §7.

use std::sync::Arc;

use bevy::asset::io::Reader;
use bevy::asset::{Asset, AssetLoader, LoadContext};
use bevy::prelude::*;

// The M1a CPU-copy floor machinery (gated by the `floor` feature). The frozen public
// types further down need NONE of this — only the floor's `Image` allocation + the
// native-render systems do — so `--no-default-features --features zero_copy` compiles
// the frozen API + zero-copy tier without pulling `wgpu-types` or these systems.
#[cfg(feature = "floor")]
use bevy::asset::RenderAssetUsages;
#[cfg(feature = "floor")]
use rive_renderer::{Artboard, Context, RenderTarget, StateMachine};
#[cfg(feature = "floor")]
use std::collections::{HashMap, HashSet};
#[cfg(feature = "floor")]
use wgpu_types::{Extent3d, TextureDimension, TextureFormat};

// At least one rendering tier must be selected, or the crate is an inert set of
// component definitions. Fail with a clear message rather than a baffling empty build.
#[cfg(not(any(feature = "floor", feature = "zero_copy")))]
compile_error!(
    "bevy-rive needs a rendering tier: enable `floor` (default, the CPU-copy plugin) \
     and/or `zero_copy` (the opt-in Vulkan fast path). A bare `--no-default-features` \
     build with neither has no renderer."
);

// M1b zero-copy Vulkan tier (gated). Re-exports the plugin + the device-sharing
// entry point; the frozen M1a ECS API above is unchanged and reused.
#[cfg(feature = "zero_copy")]
mod zero_copy;
#[cfg(feature = "zero_copy")]
pub use zero_copy::{install_interlock_device_callback, RiveGraphAnchor, RiveZeroCopyPlugin};

/// The conventional one-glob import for consumers: `use bevy_rive::prelude::*;`.
///
/// This is the whole public surface a game touches. The floor's three-step flow —
/// [`RivePlugin`] + the [`RiveAnimation`]/[`RiveTarget`] components — plus the
/// [`RiveFile`] asset and its selectors. With the `zero_copy` feature it additionally
/// re-exports [`RiveZeroCopyPlugin`] and [`install_interlock_device_callback`]; the
/// GPU-interop machinery (render-graph node, `as_hal` extraction, watermark) stays
/// private. See the crate docs for each tier's setup.
pub mod prelude {
    // Frozen components + asset — spawned identically by BOTH tiers.
    pub use crate::{
        ArtboardSelector, RiveActive, RiveAnimation, RiveAtlasKey, RiveFile, RiveSampling,
        RiveSurface, RiveTarget, StateMachineSelector,
    };

    // Floor (default): the CPU-copy plugin entry point.
    #[cfg(feature = "floor")]
    pub use crate::RivePlugin;

    // Zero-copy (opt-in): the Vulkan fast-path plugin + its graph anchor + device wiring.
    #[cfg(feature = "zero_copy")]
    pub use crate::{install_interlock_device_callback, RiveGraphAnchor, RiveZeroCopyPlugin};
}

/// The texture format M1a allocates the [`RiveTarget::image`] in.
///
/// **Not** part of the frozen surface and intentionally `pub(crate)`: the pixel
/// format is a per-tier allocation choice (M1b may wrap rive's `VkImage` in a
/// different wgpu format). Consumers that need the format must read it off the
/// allocated [`Image`] (`image.texture_descriptor.format`), never hard-code it.
#[cfg(feature = "floor")]
pub(crate) const RIVE_TEXTURE_FORMAT: TextureFormat = TextureFormat::Rgba8UnormSrgb;

/// The straight-RGBA clear rive renders behind the artboard. Default: opaque dark
/// gray (`0x303030`), matching the M0/M1.0 PNG references — an opaque clear makes
/// premultiplied == straight, so the composite is reference-exact. The alpha is a
/// **test knob** via `RIVE_CLEAR_ALPHA` (default `1.0`): `0.0` clears to transparent
/// so antialiased edges + soft fills become partial-alpha, exercising the
/// un-premultiply `c/a` divide that an opaque clear never reaches. Read once and
/// shared by the M1a CPU path and the M1b zero-copy node, so both clear identically.
#[cfg(any(feature = "floor", feature = "zero_copy"))]
pub(crate) fn rive_clear_rgba() -> [f32; 4] {
    use std::sync::OnceLock;
    static CLEAR: OnceLock<[f32; 4]> = OnceLock::new();
    *CLEAR.get_or_init(|| {
        let a = std::env::var("RIVE_CLEAR_ALPHA")
            .ok()
            .and_then(|s| s.parse::<f32>().ok())
            .unwrap_or(1.0)
            .clamp(0.0, 1.0);
        [0.188, 0.188, 0.188, a]
    })
}

// ---------------------------------------------------------------------------
// Plugin.
// ---------------------------------------------------------------------------

/// Registers the `.riv` asset + loader, the [`RiveMaterial`] display surface, and
/// the four main-thread systems that drive the CPU-copy bridge.
///
/// `DefaultPlugins` (hence `AssetPlugin`, `RenderPlugin`, `Core2dPlugin`) must be
/// added **before** `RivePlugin`; the color contract relies on `Core2dPlugin`
/// registering `Tonemapping::None` on `Camera2d`.
#[derive(Debug, Default)]
pub struct RivePlugin;

impl RivePlugin {
    /// Registers the `.riv` [`RiveFile`] asset + its [`RivLoader`] on `app`.
    ///
    /// Factored out so the M1b [`RiveZeroCopyPlugin`] can register the exact same
    /// asset + loader without also adding the M1a CPU-copy systems (which would
    /// double-drive M1b entities). Idempotent: `init_asset` is a no-op if already
    /// registered.
    pub(crate) fn register_asset(app: &mut App) {
        app.init_asset::<RiveFile>()
            .register_asset_loader(RivLoader);
    }
}

// The M1a CPU-copy floor plugin. Gated by `floor` (default): a `zero_copy`-only build
// uses [`RiveZeroCopyPlugin`] and never compiles these systems. The `RivePlugin` struct
// + `register_asset` stay always-on (the zero-copy tier reuses `register_asset`).
#[cfg(feature = "floor")]
impl Plugin for RivePlugin {
    fn build(&self, app: &mut App) {
        // (1) Asset store + AssetEvent<RiveFile> + the `.riv` loader.
        Self::register_asset(app);

        // (2) NonSend machinery (main-thread only). The Vulkan Context is created
        //     lazily on first use, so `build` is infallible and touches no GPU.
        app.init_non_send_resource::<RiveContext>()
            .init_non_send_resource::<RiveInstances>();

        // (3) Main-thread systems, chained so a handle written this frame is
        //     visible to the next system the same frame.
        app.add_systems(
            Update,
            (
                instantiate_rive_instances,
                advance_and_upload_rive,
                resize_rive_targets,
                cleanup_despawned_instances,
            )
                .chain(),
        );
    }
}

// ---------------------------------------------------------------------------
// FROZEN: the `.riv` asset + loader.
// ---------------------------------------------------------------------------

/// Raw, un-parsed `.riv` bytes, loaded through Bevy's asset system.
///
/// The native `rive::File` is built later, per-entity, on the main thread from
/// the `!Send` [`Context`], so this `Send + Sync` asset only carries bytes.
/// `Arc<[u8]>` keeps fan-out (one file on many entities) cheap.
#[derive(Asset, TypePath, Debug, Clone)]
pub struct RiveFile {
    /// The verbatim `.riv` file contents.
    pub bytes: Arc<[u8]>,
}

/// [`AssetLoader`] for `.riv` files. Reads the bytes verbatim; format validation
/// happens on the main thread when the native file is instantiated (the loader
/// runs on the async pool and has no [`Context`]).
#[derive(Debug, Default, TypePath)]
pub struct RivLoader;

impl AssetLoader for RivLoader {
    type Asset = RiveFile;
    type Settings = ();
    type Error = std::io::Error;

    async fn load(
        &self,
        reader: &mut dyn Reader,
        _settings: &Self::Settings,
        _load_context: &mut LoadContext<'_>,
    ) -> Result<RiveFile, Self::Error> {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await?;
        Ok(RiveFile {
            bytes: bytes.into(),
        })
    }

    fn extensions(&self) -> &[&str] {
        &["riv"]
    }
}

// ---------------------------------------------------------------------------
// FROZEN: public components.
// ---------------------------------------------------------------------------

/// What to play. M1a uses the **default** artboard + default state machine; the
/// selector fields are reserved so named selection is an additive change later.
///
/// `#[non_exhaustive]`: construct via [`RiveAnimation::new`] (then set public
/// fields), so future fields (e.g. `fit`, `alignment`, `paused`) stay additive.
#[derive(Component, Debug, Clone)]
#[require(RiveTarget)]
#[non_exhaustive]
pub struct RiveAnimation {
    /// The loaded `.riv` asset.
    pub handle: Handle<RiveFile>,
    /// Which artboard to instantiate (M1a honors only [`ArtboardSelector::Default`]).
    pub artboard: ArtboardSelector,
    /// Which scene/state machine to play (M1a honors only [`StateMachineSelector::Default`]).
    pub state_machine: StateMachineSelector,
    /// Playback speed multiplier applied to `Time::delta` (`1.0` == realtime).
    pub speed: f32,
}

impl RiveAnimation {
    /// Plays a `.riv`'s default artboard + default state machine at realtime speed.
    #[must_use]
    pub fn new(handle: Handle<RiveFile>) -> Self {
        Self {
            handle,
            artboard: ArtboardSelector::Default,
            state_machine: StateMachineSelector::Default,
            speed: 1.0,
        }
    }
}

/// Selects an artboard. Additively extensible; M1a only matches `Default`.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub enum ArtboardSelector {
    /// The file's default artboard.
    #[default]
    Default,
    // Future (additive): ByName(String), ByIndex(usize)
}

/// Selects a scene/state machine. Additively extensible; M1a only matches `Default`.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub enum StateMachineSelector {
    /// The default state machine (else first animation, else static scene).
    #[default]
    Default,
    // Future (additive): ByName(String), ByIndex(usize)
}

/// Offscreen render configuration: the pixel size and the [`Image`] the renderer
/// fills each frame.
///
/// Backend-agnostic — names nothing about Vulkan/CPU-copy/zero-copy. Whether the
/// `image` is CPU-backed (M1a) or a GPU-shared texture (M1b) is not part of this
/// contract; only its format/premultiplied/upright convention is (see crate docs).
///
/// `#[non_exhaustive]`: construct via [`RiveTarget::new`]/[`RiveTarget::atlased`]
/// (then set fields), so future fields stay additive.
#[derive(Component, Debug, Clone)]
#[non_exhaustive]
pub struct RiveTarget {
    /// Offscreen width in pixels.
    pub width: u32,
    /// Offscreen height in pixels.
    pub height: u32,
    /// The texture the renderer writes each frame. [`Handle::default`] means
    /// "plugin allocates one for me"; the plugin writes the real handle back on
    /// first instantiation.
    ///
    /// Frozen: this `Handle<Image>` is the seam later tiers reuse. **Not** frozen:
    /// whether the `Image` is CPU-readable. M1a keeps `data` resident (so capture
    /// tools can read it); M1b's zero-copy texture has `data: None`. Do not rely
    /// on `image.data` being `Some` in production code. (For an atlased face — see
    /// [`Self::atlas`] — the plugin writes a [`RiveSurface`] back instead; sample
    /// that.)
    pub image: Handle<Image>,
    /// Opt-in (M-SCALE): when `Some`, this face shares ONE atlas texture with other
    /// faces of the same [`RiveAtlasKey`] instead of a dedicated image, so the
    /// zero-copy tier renders many faces in ONE pass (the thousands-of-faces path).
    /// The plugin then writes a [`RiveSurface`] back (the shared atlas handle + this
    /// face's `uv_rect`); sample THAT, not `image`. `None` (default) keeps the
    /// dedicated-image behavior, so existing consumers are byte-for-byte unaffected.
    /// Honored only by the `zero_copy` tier on Vulkan; ignored by the floor tier.
    pub atlas: Option<RiveAtlasKey>,
}

impl RiveTarget {
    /// A `width`x`height` target whose image the plugin allocates on first use
    /// (dedicated image; not atlased).
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            image: Handle::default(),
            atlas: None,
        }
    }

    /// Like [`Self::new`] but opts this face into the shared atlas `key` (M-SCALE).
    /// The plugin writes a [`RiveSurface`] back; sample its `uv_rect` of the atlas.
    #[must_use]
    pub fn atlased(width: u32, height: u32, key: RiveAtlasKey) -> Self {
        Self {
            width,
            height,
            image: Handle::default(),
            atlas: Some(key),
        }
    }
}

impl Default for RiveTarget {
    fn default() -> Self {
        Self::new(512, 512)
    }
}

/// Groups faces that share one atlas texture (M-SCALE). Same key → same atlas;
/// different keys (e.g. per resolution/LOD bucket) use separate atlases.
/// `RiveAtlasKey::default()` (`0`) is the zero-ceremony single pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct RiveAtlasKey(pub u32);

/// Plugin **output** for an atlased face (M-SCALE): written back next to
/// [`RiveTarget`] once a slot is assigned, and the canonical thing an atlased
/// consumer samples. `image` is the shared atlas **page** this face was packed into
/// (every face sharing that page shares the handle); `uv_rect` is THIS face's normalized
/// `[0, 1]` sub-rect of it. Straight-alpha `Rgba8UnormSrgb`, exactly like the dedicated
/// [`RiveTarget::image`].
///
/// 3D: set `StandardMaterial.uv_transform` from `uv_rect` ([`RiveSampling::uv_transform`]).
/// 2D: set `Sprite.rect` ([`RiveSampling::sprite_rect`]). A non-atlased face has no
/// `RiveSurface` (sample [`RiveTarget::image`] as before).
///
/// **Re-sync is MANDATORY, not one-shot.** This face's `uv_rect` can CHANGE after a cull or
/// LOD repack, and the component is REMOVED while the face is culled (its tile is freed and
/// may be handed to another face). A consumer that latches the sampling once will then
/// mis-sample (a stale or reused tile). Drive sampling from BOTH `Changed<RiveSurface>`
/// (re-point) AND `RemovedComponents<RiveSurface>` (hide the display) — see the
/// `resync_atlas_sprites` system in the `sprite_riv_zerocopy` example.
#[derive(Component, Debug, Clone)]
pub struct RiveSurface {
    /// The shared atlas page image (same handle for every face packed into that page).
    pub image: Handle<Image>,
    /// This face's sub-rect of the atlas page, normalized `[0, 1]` (origin + size).
    pub uv_rect: Rect,
    /// The atlas page's pixel size (for `Sprite.rect = uv_rect * atlas_size`).
    pub atlas_size: UVec2,
}

/// Cull flag for a rive face (M-SCALE Phase 3). `RiveActive(false)` **pauses** the face: it
/// is not advanced and not rendered this frame, but its rive state machine is PRESERVED, so
/// reactivation **resumes in place** (no restart, no `.riv` re-parse). An atlas face also
/// **frees its tile** while paused; reactivation re-allocates a (possibly different) tile, so
/// atlas consumers MUST re-read [`RiveSurface`] on `Changed` and hide on its removal (see
/// [`RiveSampling`]). A **dedicated** (non-atlas) face has no tile and no `RiveSurface`: while
/// paused its display image simply **freezes** on its last rendered frame (no auto-clear, no
/// hide signal) — the consumer must toggle the display entity's `Visibility` itself.
/// Absent is treated as active, so existing consumers are unaffected.
///
/// This is the engine-side hook for game LOD (Tier A): the game decides which faces are
/// live and writes `RiveActive`. There is no automatic frustum cull here — the rive face is
/// decoupled from its display entity (the two-entity pattern), so it has no `ViewVisibility`;
/// a consumer can drive this from the display sprite's `ViewVisibility` if it wants engine
/// culling. Honored by the `zero_copy` tier; the floor tier ignores it.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct RiveActive(pub bool);

impl Default for RiveActive {
    fn default() -> Self {
        Self(true)
    }
}

/// Maps a [`RiveSurface`] to a consumer's sampling parameters. An atlas face occupies a
/// sub-rect of a SHARED page, so a consumer cannot bind the whole texture — it sets the 2D
/// `Sprite.rect` or the 3D `StandardMaterial.uv_transform` from these. For a non-atlas face
/// (no `RiveSurface`), sample the dedicated [`RiveTarget::image`] with no transform.
///
/// Run these on `Changed<RiveSurface>` (not once): a culled-then-reactivated face, or a LOD
/// repack, hands the consumer a NEW `uv_rect`, and a single-shot latch would strand a stale
/// (often full-page) sub-rect → permanent mis-sample.
#[derive(Debug)]
pub struct RiveSampling;

impl RiveSampling {
    /// 2D: the pixel `Rect` for `Sprite.rect` — this face's inset tile within the page.
    #[must_use]
    pub fn sprite_rect(surface: &RiveSurface) -> Rect {
        let size = surface.atlas_size.as_vec2();
        Rect {
            min: surface.uv_rect.min * size,
            max: surface.uv_rect.max * size,
        }
    }

    /// 3D: the `StandardMaterial.uv_transform` ([`bevy::math::Affine2`]) mapping a mesh's
    /// full-`[0, 1]` UVs onto this face's tile — `uv' = uv_rect.size()·uv + uv_rect.min`.
    /// One line on a shared full-UV quad mesh; no custom material, no per-face mesh.
    #[must_use]
    pub fn uv_transform(surface: &RiveSurface) -> bevy::math::Affine2 {
        let r = surface.uv_rect;
        bevy::math::Affine2::from_scale_angle_translation(r.size(), 0.0, r.min)
    }
}

// ---------------------------------------------------------------------------
// Internal (NON-frozen): NonSend machinery. M1a `floor` fill mechanism only —
// every item below is `#[cfg(feature = "floor")]`, so a `zero_copy`-only build
// compiles none of it (and never pulls `wgpu-types`).
// ---------------------------------------------------------------------------

/// Holds the single self-managed Vulkan [`Context`]. `!Send`. Created lazily on
/// first instantiate so `Plugin::build` cannot fail and no GPU is touched early.
///
/// Tri-state so a failed creation (e.g. a headless box with no Vulkan device) is
/// terminal: the plugin attempts device creation **at most once** and logs once,
/// rather than re-running it (and re-logging) every frame.
#[cfg(feature = "floor")]
#[derive(Default)]
enum RiveContext {
    /// Creation not yet attempted.
    #[default]
    Uninit,
    /// Creation failed once; do not retry.
    Failed,
    /// Ready to use.
    Ready(Context),
}

#[cfg(feature = "floor")]
impl RiveContext {
    /// Attempts creation at most once, then returns the context if ready.
    fn get_or_init(&mut self) -> Option<&Context> {
        if matches!(self, RiveContext::Uninit) {
            *self = match Context::new() {
                Ok(ctx) => RiveContext::Ready(ctx),
                Err(e) => {
                    error!("rive: failed to create Vulkan context (disabling rive): {e}");
                    RiveContext::Failed
                }
            };
        }
        self.get()
    }

    /// The context if ready, without attempting creation.
    fn get(&self) -> Option<&Context> {
        match self {
            RiveContext::Ready(ctx) => Some(ctx),
            RiveContext::Uninit | RiveContext::Failed => None,
        }
    }
}

/// One entity's native render state. `!Send`. The wrapper's `Rc` graph makes
/// field drop order non-load-bearing.
#[cfg(feature = "floor")]
struct RiveInstance {
    artboard: Artboard,
    state_machine: StateMachine,
    target: RenderTarget,
    /// Reused readback scratch (`w*h*4`); avoids a per-frame allocation.
    readback: Vec<u8>,
}

/// Per-entity native instances, keyed by [`Entity`]. `!Send`. Links the
/// `Send + Sync` components to the `!Send` native state (which cannot be a
/// component).
#[cfg(feature = "floor")]
#[derive(Default)]
struct RiveInstances {
    map: HashMap<Entity, RiveInstance>,
    /// Entities whose `.riv` could not be instantiated (corrupt/unsupported file,
    /// or an invalid size). Recorded so a permanent failure is not retried or
    /// re-logged every frame; cleared when the entity is despawned.
    failed: HashSet<Entity>,
}

/// Builds the native objects for one entity. `File` is derived then dropped — the
/// `Artboard` keeps the underlying file data alive via the wrapper's `Rc` graph.
#[cfg(feature = "floor")]
fn build_instance(
    ctx: &Context,
    bytes: &[u8],
    width: u32,
    height: u32,
) -> rive_renderer::Result<(Artboard, StateMachine, RenderTarget)> {
    let file = ctx.load_file(bytes)?;
    let artboard = file.default_artboard()?;
    let state_machine = artboard.default_state_machine()?;
    let target = ctx.offscreen_target(width, height)?;
    Ok((artboard, state_machine, target))
}

/// Advance → render → flush → readback for one instance. Disjoint field borrows
/// keep the transient `Frame` borrow scoped to this call.
#[cfg(feature = "floor")]
fn render_instance(ctx: &Context, inst: &mut RiveInstance) -> rive_renderer::Result<()> {
    let frame = ctx.begin_frame(&inst.target, rive_clear_rgba())?;
    frame.draw(&inst.artboard)?;
    frame.flush()?;
    inst.target.read_pixels(&mut inst.readback)?;
    // rive outputs premultiplied alpha; convert to straight so a straight-alpha
    // `Sprite` (the M1a display) composites correctly for partial alpha too. A
    // no-op for the opaque references. (M1b keeps premultiplied for its material.)
    rive_renderer::unpremultiply_rgba8(&mut inst.readback);
    Ok(())
}

/// Allocates a fresh CPU-backed Rive [`Image`] in the frozen format.
///
/// `MAIN_WORLD | RENDER_WORLD` keeps `data` resident so the per-frame CPU copy can
/// overwrite it (M1a fill detail — M1b allocates `RENDER_WORLD`-only, `data: None`).
#[cfg(feature = "floor")]
fn make_rive_image(width: u32, height: u32) -> Image {
    let size = Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };
    let data = vec![0u8; width as usize * height as usize * 4];
    Image::new(
        size,
        TextureDimension::D2,
        data,
        RIVE_TEXTURE_FORMAT,
        RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
    )
}

// ---------------------------------------------------------------------------
// Systems (all NonSend-pinned to the main thread).
// ---------------------------------------------------------------------------

/// Creates the native instance for each entity whose `.riv` has finished loading
/// and that has no instance yet; allocates the [`Image`] if the target's handle
/// is default and writes it back.
#[cfg(feature = "floor")]
fn instantiate_rive_instances(
    mut rive_ctx: NonSendMut<RiveContext>,
    mut instances: NonSendMut<RiveInstances>,
    mut query: Query<(Entity, &RiveAnimation, &mut RiveTarget)>,
    files: Res<Assets<RiveFile>>,
    mut images: ResMut<Assets<Image>>,
) {
    for (entity, anim, mut target) in &mut query {
        if instances.map.contains_key(&entity) || instances.failed.contains(&entity) {
            continue; // already built, or permanently failed
        }
        let Some(file_asset) = files.get(&anim.handle) else {
            continue; // not loaded yet
        };
        let Some(ctx) = rive_ctx.get_or_init() else {
            continue; // GPU init failed (already logged once)
        };

        // M1a: ArtboardSelector::Default / StateMachineSelector::Default only.
        let (artboard, state_machine, rt) =
            match build_instance(ctx, &file_asset.bytes, target.width, target.height) {
                Ok(parts) => parts,
                Err(e) => {
                    // Terminal for this entity (corrupt/unsupported file or bad
                    // size): record it so we don't retry/re-log every frame.
                    warn!("rive: cannot instantiate entity {entity:?} (giving up): {e}");
                    instances.failed.insert(entity);
                    continue;
                }
            };

        if target.image == Handle::default() {
            target.image = images.add(make_rive_image(target.width, target.height));
        }

        let readback = vec![0u8; rt.pixel_buffer_size()];
        instances.map.insert(
            entity,
            RiveInstance {
                artboard,
                state_machine,
                target: rt,
                readback,
            },
        );
    }
}

/// The per-frame core: advance each state machine by `Time::delta * speed`, render
/// it offscreen, read the pixels back, and copy them into the target [`Image`].
#[cfg(feature = "floor")]
fn advance_and_upload_rive(
    rive_ctx: NonSend<RiveContext>,
    mut instances: NonSendMut<RiveInstances>,
    time: Res<Time>,
    mut images: ResMut<Assets<Image>>,
    query: Query<(Entity, &RiveAnimation, &RiveTarget)>,
) {
    let Some(ctx) = rive_ctx.get() else {
        return;
    };
    let dt = time.delta_secs();
    for (entity, anim, target) in &query {
        let Some(inst) = instances.map.get_mut(&entity) else {
            continue;
        };

        // Guard the native state machine against NaN/negative/non-finite steps
        // (`speed` is user-controlled). Relies on `Time` being virtual-clamped
        // (~250 ms max) to bound a huge real delta.
        let step = dt * anim.speed;
        if step.is_finite() {
            inst.state_machine.advance(step.max(0.0));
        }
        if let Err(e) = render_instance(ctx, inst) {
            warn!("rive: frame failed for {entity:?}: {e}");
            continue;
        }

        // M1a fill: `get_mut` (tracked) queues `AssetEvent::Modified`, which makes
        // the render world re-upload the texture.
        if let Some(image) = images.get_mut(&target.image) {
            if let Some(dst) = image.data.as_mut() {
                if dst.len() == inst.readback.len() {
                    dst.copy_from_slice(&inst.readback);
                }
            }
        }
    }
}

/// Recreates the offscreen target + [`Image`] when a [`RiveTarget`]'s size changes.
#[cfg(feature = "floor")]
fn resize_rive_targets(
    rive_ctx: NonSend<RiveContext>,
    mut instances: NonSendMut<RiveInstances>,
    query: Query<(Entity, &RiveTarget), Changed<RiveTarget>>,
    mut images: ResMut<Assets<Image>>,
) {
    let Some(ctx) = rive_ctx.get() else {
        return;
    };
    for (entity, target) in &query {
        let Some(inst) = instances.map.get_mut(&entity) else {
            continue; // not instantiated yet (handled next frame)
        };
        if inst.target.width() == target.width && inst.target.height() == target.height {
            continue; // only the handle changed (e.g. first write-back)
        }
        match ctx.offscreen_target(target.width, target.height) {
            Ok(rt) => {
                inst.readback = vec![0u8; rt.pixel_buffer_size()];
                inst.target = rt;
                if let Some(image) = images.get_mut(&target.image) {
                    *image = make_rive_image(target.width, target.height);
                }
            }
            Err(e) => warn!("rive: resize failed for {entity:?}: {e}"),
        }
    }
}

/// Drops native instances whose entity was despawned or lost its [`RiveAnimation`].
/// Runs on the main thread (the system is `NonSend`-pinned), as the `!Send`
/// destructors require.
#[cfg(feature = "floor")]
fn cleanup_despawned_instances(
    mut instances: NonSendMut<RiveInstances>,
    alive: Query<Entity, With<RiveAnimation>>,
) {
    if instances.map.is_empty() && instances.failed.is_empty() {
        return;
    }
    let live: HashSet<Entity> = alive.iter().collect();
    instances.map.retain(|entity, _| live.contains(entity));
    instances.failed.retain(|entity| live.contains(entity));
}
