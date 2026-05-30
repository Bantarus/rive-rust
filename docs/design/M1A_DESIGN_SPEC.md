All claims now verified against the 0.18.1 registry. `Hdr` lives in `bevy_render::view`. `ColorTargetState.blend: Option<BlendState>`. I have everything needed to produce the final corrected spec.

# bevy-rive — M1a Final Design Spec (Bevy 0.18.1, CPU-copy bridge)

> Status: **implementation-ready**. Grounded in the **actual** Bevy 0.18.1 registry source (every API path/signature below was verified against `/home/bantarus/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/bevy*-0.18.1`, file:line cited) and the post-refactor Rc-owned `rive-renderer` wrapper (every handle `'static`, `!Send + !Sync`). Reference PNGs `out.png` / `out_octopus.png` are 512×512 RGBA, opaque clear `0x303030ff`.
>
> **FROZEN surface (M1b zero-copy Vulkan + M2/M3 reuse these verbatim):** the §1 public API (plugin, asset, loader, components, `RiveMaterial`, systems), and the **backend-agnostic** color/orientation facts in §3.0. Everything in §2 (NonSend machinery, `RenderAssetUsages`, the `images.get_mut` → `AssetEvent::Modified` re-upload path, "`data` stays `Some`") is **M1a-internal fill mechanism, NOT frozen** — M1b replaces it with a `RENDER_WORLD`-only shared `VkImage` (`data: None`) and no CPU copy.

---

## 0. The M0 wrapper API this sits on

All handles `'static`, `!Send + !Sync`:

```
Context::new() -> Result<Context>
ctx.offscreen_target(w: u32, h: u32) -> Result<RenderTarget>
ctx.load_file(&[u8]) -> Result<File>
file.default_artboard() -> Result<Artboard>
artboard.default_state_machine() -> Result<StateMachine>   // sm.advance(&mut self, dt: f32)
ctx.begin_frame(&target, clear_rgba: [f32;4]) -> Result<Frame<'a>>   // BORROWS ctx + target
frame.draw(&artboard) -> Result<()>;  frame.flush(self) -> Result<()>   // flush CONSUMES Frame
target.read_pixels(&mut [u8]) -> Result<()>   // top-down RGBA8, sRGB bytes, PREMULTIPLIED
target.pixel_buffer_size() -> usize;  target.width()/height() -> u32
unpremultiply_rgba8(&mut [u8])   // helper, intentionally NOT called in this plugin
```

> **Critical lifetime note:** `Frame<'a>` borrows both `&Context` and `&RenderTarget`. Never store a `Frame`. Create→draw→flush in one tight block: `let frame = ctx.begin_frame(&target, clear)?; frame.draw(&ab)?; frame.flush()?;`. `flush` takes `self` by value.

---

## 1. FROZEN public API

`use bevy::prelude::*;` brings `Asset`, `AssetApp`, `Handle`, `Component`, `MessageWriter`, `Mesh2d`, `MeshMaterial2d`, etc. into scope. **It does NOT bring `Material2d` / `Material2dPlugin` / `Material2dKey` / `AlphaMode2d`** — those require the explicit `bevy::sprite_render::` path (verified: `bevy_sprite_render-0.18.1/src/lib.rs` prelude only re-exports `ColorMaterial`, `MeshMaterial2d`; `Mesh2d`/`Mesh3d` come from `bevy_mesh` prelude).

### 1.1 `RivePlugin`

```rust
use bevy::prelude::*;
use bevy::asset::embedded_asset;
use bevy::sprite_render::Material2dPlugin;

#[derive(Default)]
pub struct RivePlugin;

impl Plugin for RivePlugin {
    fn build(&self, app: &mut App) {
        // (1) Asset store + AssetEvent<RiveFile> + loader.
        app.init_asset::<RiveFile>()
            .register_asset_loader(RivLoader);

        // (2) The frozen display material + its embedded WGSL.
        //     Path string becomes "embedded://bevy_rive/rive_material.wgsl".
        embedded_asset!(app, "rive_material.wgsl");
        app.add_plugins(Material2dPlugin::<RiveMaterial>::default());

        // (3) NonSend machinery (main-thread only). Context created LAZILY on first
        //     instantiate run, so Plugin::build never fails and no GPU is touched here.
        app.init_non_send_resource::<RiveContext>()     // holds Option<Context>
            .init_non_send_resource::<RiveInstances>();  // HashMap<Entity, RiveInstance>

        // (4) Four chained Update systems, all NonSend-pinned to the main thread.
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
```

> **Precondition (documented):** `DefaultPlugins` (hence `AssetPlugin`, `RenderPlugin`, **`Core2dPlugin`**) must be added **before** `RivePlugin`: `app.add_plugins(DefaultPlugins).add_plugins(RivePlugin)`. The color contract (§3) depends on `Core2dPlugin` registering `Tonemapping::None` on `Camera2d` — see §3.3.

### 1.2 The `.riv` asset + loader

Mirrors the 0.18.1 `bevy_audio` `AudioSource`/`AudioLoader` template. `Arc<[u8]>` so one `.riv` instanced onto many entities clones cheaply (both `Arc<[u8]>` and `Vec<u8>` are `Send+Sync`, satisfying `Asset`).

```rust
use bevy::asset::io::Reader;
use bevy::asset::{Asset, AssetLoader, LoadContext};
use bevy::prelude::TypePath;
use std::sync::Arc;

/// Raw, un-parsed `.riv` bytes. The native `rive::File` is built later, per-entity,
/// on the main thread from the !Send Context — so this Send+Sync asset only carries bytes.
#[derive(Asset, TypePath, Debug, Clone)]
pub struct RiveFile {
    pub bytes: Arc<[u8]>,
}

#[derive(Default, TypePath)]
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
        Ok(RiveFile { bytes: bytes.into() })
    }

    fn extensions(&self) -> &[&str] { &["riv"] }
}
```

No `.riv`-format validation at load time (loader runs on the async pool, no `Context`); format errors surface on the main thread when `ctx.load_file` runs, logged via `warn!` (§2.1). `std::io::Error` satisfies `Into<BevyError>`.

### 1.3 Public components

Two components on the same entity, **not** a bundle (separation of input vs output config; `RiveTarget` attachable/swappable independently). `RiveAnimation` declares `RiveTarget` as a required component for bundle-like spawn ergonomics.

```rust
use bevy::prelude::*;

/// FROZEN. Selects what to play. M1a uses the *default* artboard + default state machine;
/// the selector fields are reserved so named selection is an ADDITIVE future change
/// (new variants on a #[non_exhaustive] enum + reading a currently-ignored field).
#[derive(Component, Debug, Clone)]
#[require(RiveTarget)]
pub struct RiveAnimation {
    pub handle: Handle<RiveFile>,
    pub artboard: ArtboardSelector,           // M1a honors only `Default`
    pub state_machine: StateMachineSelector,  // M1a honors only `Default`
    pub speed: f32,                           // reserved knob; 1.0 = realtime
}

impl RiveAnimation {
    pub fn new(handle: Handle<RiveFile>) -> Self {
        Self {
            handle,
            artboard: ArtboardSelector::Default,
            state_machine: StateMachineSelector::Default,
            speed: 1.0,
        }
    }
}

/// FROZEN, additively-extensible. M1a only matches `Default`.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub enum ArtboardSelector {
    #[default]
    Default,
    // Future (additive): ByName(String), ByIndex(usize)
}

/// FROZEN, additively-extensible. M1a only matches `Default`.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub enum StateMachineSelector {
    #[default]
    Default,
    // Future (additive): ByName(String), ByIndex(usize)
}

/// FROZEN. Offscreen render config: pixel size + the `Image` the renderer fills each
/// frame. Backend-agnostic — names nothing about Vulkan/CPU-copy/zero-copy. Whether the
/// `image` is CPU-backed (M1a) or a GPU-shared VkImage (M1b) is NOT part of this contract.
#[derive(Component, Debug, Clone)]
pub struct RiveTarget {
    pub width: u32,
    pub height: u32,
    /// The texture the renderer writes each frame. `Handle::default()` means
    /// "plugin allocates one for me" (in the frozen format, see RIVE_TEXTURE_FORMAT);
    /// the plugin writes the real handle back on first instantiation.
    pub image: Handle<Image>,
}

impl Default for RiveTarget {
    fn default() -> Self {
        Self { width: 512, height: 512, image: Handle::default() }
    }
}
```

> **Naming note (frozen, accepted asymmetry):** `RiveTarget.image` vs `RiveMaterial.texture`. The critique flagged aligning these. We keep `image` because the field's *type* is `Handle<Image>` and the component is the render *target*; `RiveMaterial.texture` follows the wgpu/`AsBindGroup` `#[texture(0)]` convention. The asymmetry is cosmetic and permanent; documented here so it is a deliberate choice, not an oversight.

The public `Handle<Image>` is the seam M1b reuses unchanged: M1a fills it via CPU copy; M1b swaps the backing `VkImage`. The component, the format contract, and `RiveMaterial` never change.

### 1.4 Public display material (FROZEN consuming type) — CORRECTED IMPORTS

> **The draft's `use bevy::sprite::{...}` paths were a hard compile error (E0432).** Verified correct paths: `Material2d`/`Material2dPlugin`/`Material2dKey`/`AlphaMode2d` ⇒ `bevy::sprite_render::*` (`bevy_sprite_render-0.18.1/src/mesh2d/material.rs`); `MeshVertexBufferLayoutRef` ⇒ `bevy::mesh::*` (the trait itself does `use bevy_mesh::MeshVertexBufferLayoutRef;`, material.rs:24); `AsBindGroup`/`RenderPipelineDescriptor`/`BlendState`/`SpecializedMeshPipelineError`/`ColorTargetState` ⇒ `bevy::render::render_resource::*`; `ShaderRef` ⇒ `bevy::shader::*` (it is **NOT** re-exported through `render_resource` — the draft's claim was false). The `specialize` signature takes params **by value as the trait declares** (material.rs:160-164): `descriptor: &mut RenderPipelineDescriptor, layout: &MeshVertexBufferLayoutRef, key: Material2dKey<Self>` (no leading underscore on the public trait fn name).

```rust
use bevy::prelude::*;
use bevy::render::render_resource::{
    AsBindGroup, BlendState, RenderPipelineDescriptor, SpecializedMeshPipelineError,
};
use bevy::mesh::MeshVertexBufferLayoutRef;
use bevy::shader::ShaderRef;
use bevy::sprite_render::{AlphaMode2d, Material2d, Material2dKey};

/// FROZEN. Displays a Rive-filled texture with PREMULTIPLIED-alpha blending — correct
/// for opaque (matches the reference PNG) and transparent content. M1b reuses verbatim.
#[derive(Asset, TypePath, AsBindGroup, Debug, Clone)]
pub struct RiveMaterial {
    #[texture(0)]
    #[sampler(1)]
    pub texture: Handle<Image>,
}

impl Material2d for RiveMaterial {
    fn fragment_shader() -> ShaderRef {
        // Registered via embedded_asset!(app, "rive_material.wgsl") in RivePlugin::build.
        // Crate `bevy-rive` -> module path `bevy_rive`.
        "embedded://bevy_rive/rive_material.wgsl".into()
    }

    fn alpha_mode(&self) -> AlphaMode2d {
        AlphaMode2d::Blend // -> Transparent2d phase; we override its blend in specialize().
    }

    fn specialize(
        descriptor: &mut RenderPipelineDescriptor,
        _layout: &MeshVertexBufferLayoutRef,
        _key: Material2dKey<Self>,
    ) -> Result<(), SpecializedMeshPipelineError> {
        // AlphaMode2d::Blend gives straight ALPHA_BLENDING; override to PREMULTIPLIED.
        // descriptor.fragment: Option<FragmentState>; .targets: Vec<Option<ColorTargetState>>;
        // ColorTargetState.blend: Option<BlendState>  (verified pipeline.rs / wgpu-types 27.0.1).
        if let Some(frag) = descriptor.fragment.as_mut() {
            if let Some(Some(target)) = frag.targets.get_mut(0) {
                target.blend = Some(BlendState::PREMULTIPLIED_ALPHA_BLENDING);
            }
        }
        Ok(())
    }
}
```

### 1.5 Public systems & schedule

All four run in `Update`, **chained** (apply-deferred barriers between them so a freshly-spawned/inserted handle is visible the same frame). Every system takes a `NonSend`/`NonSendMut` param ⇒ the executor pins it to the world-owning main thread (required: the wrapper's destructors are `!Send`).

| System | Params (abridged) | Role |
|---|---|---|
| `instantiate_rive_instances` | `NonSendMut<RiveContext>`, `NonSendMut<RiveInstances>`, `Query<(Entity, &RiveAnimation, &mut RiveTarget)>`, `Res<Assets<RiveFile>>`, `ResMut<Assets<Image>>` | Lazily create `Context`; for each entity whose `.riv` is loaded and has no instance, build `File→Artboard→StateMachine→RenderTarget`, allocate the `Image` if `RiveTarget.image` is default, write the handle back, insert into the map. |
| `advance_and_upload_rive` | `NonSend<RiveContext>`, `NonSendMut<RiveInstances>`, `Res<Time>`, `ResMut<Assets<Image>>`, `Query<(Entity, &RiveAnimation, &RiveTarget)>` | Per instance: `sm.advance(dt·speed)`; `begin_frame→draw→flush`; `read_pixels` into reused scratch; copy into `images.get_mut(&image).data`. |
| `resize_rive_targets` | `NonSend<RiveContext>`, `NonSendMut<RiveInstances>`, `Query<(Entity,&RiveTarget), Changed<RiveTarget>>`, `ResMut<Assets<Image>>` | On `(w,h)` change, recreate the offscreen target + the `Image`. |
| `cleanup_despawned_instances` | `NonSendMut<RiveInstances>`, `Query<Entity, With<RiveAnimation>>`, `&Entities` | Drop native instances whose entity no longer exists (drop runs on main thread). |

---

## 2. Internal (NON-FROZEN) machinery — M1a fill mechanism

> **NOT part of the frozen surface.** Everything here — `RenderAssetUsages`, "`data` stays `Some`", `images.get_mut` → `AssetEvent::Modified` → `create_texture_with_data` re-upload — is the **M1a CPU-copy fill strategy**. **M1b replaces this entirely** with a `RENDER_WORLD`-only shared `VkImage` (`data: None`, no per-frame CPU pass). The only thing the consumer (sprite/material/extract pipeline) sees across tiers is the frozen `RiveTarget.image: Handle<Image>` carrying the §3.0 format/premultiplied/upright convention.

Native objects are `!Send + !Sync` ⇒ they live only in NonSend resources keyed by `Entity` (Bevy `Component: Send + Sync + 'static`, so per-entity native state cannot be a component).

```rust
use bevy::prelude::*;
use bevy::platform::collections::HashMap;
use rive_renderer::{Artboard, Context, RenderTarget, StateMachine};

/// Holds the single self-managed Vulkan Context. !Send. Created lazily on first
/// instantiate so Plugin::build can't fail and no GPU is touched until needed.
#[derive(Default)]
struct RiveContext {
    ctx: Option<Context>,
}

impl RiveContext {
    fn get_or_init(&mut self) -> Option<&Context> {
        if self.ctx.is_none() {
            match Context::new() {
                Ok(c) => self.ctx = Some(c),
                Err(e) => { error!("rive: failed to create Vulkan context: {e}"); return None; }
            }
        }
        self.ctx.as_ref()
    }
}

/// One entity's native render state. !Send. The wrapper's Rc graph makes field drop
/// order non-load-bearing.
struct RiveInstance {
    artboard: Artboard,
    state_machine: StateMachine,
    target: RenderTarget,
    readback: Vec<u8>, // reused scratch (w*h*4); avoids a per-frame alloc.
}

/// Per-entity native instances. !Send. The Entity key links Send+Sync components to
/// !Send native state.
#[derive(Default)]
struct RiveInstances {
    map: HashMap<Entity, RiveInstance>,
}
```

**`File` is not stored:** it is only needed to call `default_artboard()`; the wrapper's `Artboard` holds an `Rc<ContextInner>` keeping the underlying file data alive, so `File` is derived-then-dropped inside the instantiate block (dropping `File` before `Artboard` is safe per the wrapper's Drop docs).

### 2.1 `instantiate_rive_instances`

```rust
fn instantiate_rive_instances(
    mut rive_ctx: NonSendMut<RiveContext>,
    mut instances: NonSendMut<RiveInstances>,
    mut query: Query<(Entity, &RiveAnimation, &mut RiveTarget)>,
    files: Res<Assets<RiveFile>>,
    mut images: ResMut<Assets<Image>>,
) {
    for (entity, anim, mut target) in &mut query {
        if instances.map.contains_key(&entity) { continue; }              // already built
        let Some(file_asset) = files.get(&anim.handle) else { continue }; // not loaded yet
        let Some(ctx) = rive_ctx.get_or_init() else { continue };         // GPU init failed

        // M1a: ArtboardSelector::Default / StateMachineSelector::Default only.
        let build = (|| -> rive_renderer::Result<(Artboard, StateMachine, RenderTarget)> {
            let file = ctx.load_file(&file_asset.bytes)?;     // File dropped at block end
            let artboard = file.default_artboard()?;
            let state_machine = artboard.default_state_machine()?;
            let rt = ctx.offscreen_target(target.width, target.height)?;
            Ok((artboard, state_machine, rt))
        })();
        let (artboard, state_machine, rt) = match build {
            Ok(t) => t,
            Err(e) => { warn!("rive: failed to instantiate entity {entity:?}: {e}"); continue; }
        };

        if target.image == Handle::default() {
            target.image = images.add(make_rive_image(target.width, target.height));
        }

        let readback = vec![0u8; rt.pixel_buffer_size()];
        instances.map.insert(entity, RiveInstance { artboard, state_machine, target: rt, readback });
    }
}
```

`files.get(&handle).is_some()` is the load-readiness gate (idempotent; re-checked each frame). The block-scoped `File` keeps the `Frame<'a>` borrow rules irrelevant here.

### 2.2 `advance_and_upload_rive` — the per-frame core

```rust
fn advance_and_upload_rive(
    rive_ctx: NonSend<RiveContext>,
    mut instances: NonSendMut<RiveInstances>,
    time: Res<Time>,
    mut images: ResMut<Assets<Image>>,
    query: Query<(Entity, &RiveAnimation, &RiveTarget)>,
) {
    let Some(ctx) = rive_ctx.ctx.as_ref() else { return };
    let dt = time.delta_secs(); // verified bevy_time-0.18.1/src/time.rs:283
    for (entity, anim, target) in &query {
        let Some(inst) = instances.map.get_mut(&entity) else { continue };

        inst.state_machine.advance(dt * anim.speed);

        // Frame borrows ctx + inst.target for THIS block only.
        let render = (|| -> rive_renderer::Result<()> {
            let frame = ctx.begin_frame(&inst.target, CLEAR_RGBA)?;
            frame.draw(&inst.artboard)?;
            frame.flush()?; // consumes Frame
            inst.target.read_pixels(&mut inst.readback)
        })();
        if let Err(e) = render { warn!("rive: frame failed for {entity:?}: {e}"); continue; }

        // M1a fill: get_mut queues AssetEvent::Modified -> render world re-uploads.
        if let Some(image) = images.get_mut(&target.image) {
            if let Some(dst) = image.data.as_mut() {
                debug_assert_eq!(dst.len(), inst.readback.len());
                dst.copy_from_slice(&inst.readback);
            }
        }
    }
}

/// §3.0 color contract. Opaque dark gray == the reference PNG clear (premult==straight at α=1).
const CLEAR_RGBA: [f32; 4] = [0.188, 0.188, 0.188, 1.0]; // 0x303030, straight (opaque)
```

`images.get_mut` (NOT `get_mut_untracked`) is mandatory for M1a: only it queues `AssetEvent::Modified`, driving re-upload. The Image is `MAIN_WORLD | RENDER_WORLD` (§2.4) so `data` (`Option<Vec<u8>>`, verified `image.rs:585`) stays `Some` every frame. **All of this is M1a-only.**

### 2.3 `resize_rive_targets` & `cleanup_despawned_instances`

```rust
fn resize_rive_targets(
    rive_ctx: NonSend<RiveContext>,
    mut instances: NonSendMut<RiveInstances>,
    query: Query<(Entity, &RiveTarget), Changed<RiveTarget>>,
    mut images: ResMut<Assets<Image>>,
) {
    let Some(ctx) = rive_ctx.ctx.as_ref() else { return };
    for (entity, target) in &query {
        let Some(inst) = instances.map.get_mut(&entity) else { continue };
        if inst.target.width() == target.width && inst.target.height() == target.height { continue; }
        match ctx.offscreen_target(target.width, target.height) {
            Ok(rt) => {
                inst.readback = vec![0u8; rt.pixel_buffer_size()];
                inst.target = rt;
                if let Some(img) = images.get_mut(&target.image) {
                    *img = make_rive_image(target.width, target.height); // recreate -> Modified
                }
            }
            Err(e) => warn!("rive: resize failed for {entity:?}: {e}"),
        }
    }
}

fn cleanup_despawned_instances(
    mut instances: NonSendMut<RiveInstances>,
    alive: Query<(), With<RiveAnimation>>,
    entities: &bevy::ecs::entity::Entities,
) {
    // Drop native instances whose entity is gone OR lost its RiveAnimation.
    instances.map.retain(|&e, _| entities.contains(e) && alive.get(e).is_ok());
    // Drop runs on the main thread (system is NonSend-pinned) — required for !Send destructors.
}
```

> `RemovedComponents<RiveAnimation>` is the cleaner idiom if preferred; `Entities::contains` + `retain` is the dependency-light fallback shown. Either way, instance drop runs main-thread.

### 2.4 `make_rive_image` — image allocation (M1a fill detail)

```rust
use bevy::asset::RenderAssetUsages;
use bevy::image::Image;
use wgpu_types::{Extent3d, TextureDimension};

fn make_rive_image(w: u32, h: u32) -> Image {
    let size = Extent3d { width: w, height: h, depth_or_array_layers: 1 };
    let data = vec![0u8; (w as usize) * (h as usize) * 4]; // RGBA8, len == w*h*4
    Image::new(
        size,
        TextureDimension::D2,
        data,
        RIVE_TEXTURE_FORMAT,                  // §3.0 frozen format (Rgba8UnormSrgb)
        // M1a fill detail (NOT frozen): MAIN_WORLD keeps `data` Some for the CPU copy.
        // M1b allocates RENDER_WORLD-only (data: None) — same FORMAT, different usages.
        RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
    )
}
```

> `Image::new` takes `wgpu_types::{Extent3d, TextureDimension, TextureFormat}` directly (verified `bevy_image-0.18.1/src/image.rs:1007`), and these are **not** re-exported through `bevy::image`. **Type-identity hazard:** the lib's `wgpu-types` MUST resolve to the exact `27.0.1` bevy pins, or `TextureFormat` is a different type and `Image::new` won't accept the const. **Pin `wgpu-types = "=27.0.1"`** in the lib `Cargo.toml` (see §5.1) and add a `cargo tree -d` CI check.

---

## 3. Colorspace + premultiplied-alpha contract — THE PRIORITY

### 3.0 The FROZEN, backend-agnostic facts (M1b reuses verbatim)

| Frozen property | Value | Why it is truly backend-agnostic |
|---|---|---|
| **`RiveTarget.image` texture format** | **`TextureFormat::Rgba8UnormSrgb`** (== `RIVE_TEXTURE_FORMAT`) | Rive output is sRGB-encoded; this is `VK_FORMAT_R8G8B8A8_SRGB`-equivalent, the same format M1b's shared VkImage carries. `TextureFormat` names no backend. |
| **Alpha storage convention** | **PREMULTIPLIED, sRGB-encoded bytes** | Rive's native output AND M1b's zero-copy VkImage bytes. `unpremultiply_rgba8` is deliberately never called. |
| **Orientation** | **Upright, top-down rows, no flip anywhere** | Shim emits canonical-upright top-down rows; identical bytes across tiers. See §4. |
| **Display blend** | **`BlendState::PREMULTIPLIED_ALPHA_BLENDING`** via `RiveMaterial` | Correct compositing for premultiplied RGB; identical to straight blending at α=1. |

> **What is NOT frozen** (moved here per the tier-freeze critique, severity major): `RenderAssetUsages`, "`data` stays `Some`", and the `images.get_mut → AssetEvent::Modified → create_texture_with_data` upload story are **M1a CPU-fill internals** (§2), not public contract. M1b uses a `RENDER_WORLD`-only shared VkImage with `data: None` and no CPU pass. The frozen seam is only: *the `Handle<Image>` carries Rgba8UnormSrgb + premultiplied + upright*; whether it is CPU-backed or GPU-shared is unspecified.

> **`RIVE_TEXTURE_FORMAT` is public but documented as "the plugin's allocation format."** Consumers that need the format should read it off the `Image`, not hardcode the const, so a future tier could allocate differently without breaking callers (minor critique, accepted).

### 3.1 The remaining (M1a-recommended, not all frozen) display setup

| Property | Value | Rationale |
|---|---|---|
| TextureUsages | default `TEXTURE_BINDING \| COPY_DST \| COPY_SRC` (set by `Image::new`) | Sufficient for sampling; do not override. |
| Sampler | `ImageSampler::Default` (linear); **nearest** for the pixel-diff run (§6) | Default fine for display. |
| Display path | **Custom `RiveMaterial` (Material2d) on a `Mesh2d` quad** | The ONLY 0.18.1 path that accepts premultiplied output with zero CPU work AND is correct for transparency. `AlphaMode2d` has **no `Premultiplied` variant** (only `Opaque`/`Mask`/`Blend`, verified material.rs:242); plain `Sprite` is hard-wired to straight `ALPHA_BLENDING` with no per-sprite override. |
| Camera | **`Camera2d::default()`, NO `Hdr` component** | See §3.3 — Tonemapping::None + non-HDR ViewTarget close the sRGB round-trip. |
| MSAA (capture/diff) | **`Msaa::Off` REQUIRED for the diff path** | Default is `Sample4` (verified `bevy_render view/mod.rs:172`); 4× MSAA blends content edges with the clear, so silhouette pixels differ from the reference even when color is correct. `Msaa::Off` makes the interior byte-exact. |

### 3.2 Why this matches the opaque reference PNG **exactly** (interior, `Msaa::Off`)

Reference clear is opaque `0x303030ff` ⇒ **premultiplied == straight**. With our path:

1. Rive writes sRGB-encoded premultiplied RGBA8 → copied byte-for-byte into the `Rgba8UnormSrgb` Image `data` (M1a) / shared directly (M1b).
2. GPU samples the `*Srgb` texture: hardware sRGB→linear decode.
3. Fragment shader returns the sample unchanged. The in-shader tonemap **does execute** (see §3.3) but is a numerical identity under `Tonemapping::None` + default `ColorGrading`.
4. `PREMULTIPLIED_ALPHA_BLENDING` composites; at α=1 this equals straight blending; over an opaque background the result is the source color exactly.
5. The non-HDR 2D `ViewTarget` is `Rgba8UnormSrgb` (`bevy_default()`): linear→sRGB re-encode on write.

Steps 2→5 are an exact sRGB decode/encode round-trip; steps 3–4 are identities for the opaque case ⇒ composited RGB equals the rive bytes equals the reference PNG (sampler/rounding only, with `Msaa::Off`).

### 3.3 Why the in-shader tonemap is an identity — CORRECTED MECHANISM

> The draft claimed "`Camera2d::default()` forces `Tonemapping::None`" and that "the in-shader path early-returns." **Both wrong** (colorspace + soundness critiques, severity major).

**Verified facts:**
- `Camera2d` requires only `Camera, Projection, Frustum` (verified `bevy_camera-0.18.1/src/components.rs:11-16`) — **NOT** `Tonemapping`.
- `Tonemapping::None` on a 2D camera comes from **`Core2dPlugin`**: `register_required_components_with::<Camera2d, Tonemapping>(|| Tonemapping::None)` (verified `bevy_core_pipeline-0.18.1/src/core_2d/mod.rs:88`). So the identity holds **only because `DefaultPlugins → Core2dPlugin` is added** (the §1.1 precondition).
- The `Tonemapping` enum's own `Default` is `TonyMcMapface`, **not** `None`. **If anything inserts a bare `Tonemapping::default()` on the 2D camera, the Tony LUT alters color.**
- The in-shader `tone_mapping()` **is compiled in and runs every frame**: `TONEMAP_IN_SHADER` is set whenever a `Tonemapping` component is present and `!view.hdr` (mesh2d path). `Camera2d` has `Tonemapping::None` *present*, so the `#ifdef TONEMAP_IN_SHADER` branch is **active** — it does not early-return. It is an identity only because `TONEMAP_METHOD_NONE → color = color`, and default `ColorGrading` makes exposure `2^0=1` and `post_saturation` `mix(luma, color, 1.0) = color`, with hue-rotate/white-balance/sectional-grading shader-defs off at defaults. (The *standalone* `TonemappingNode` does early-return for `None`; that is a separate, also-inert path.)
- **HDR precondition:** `Camera2d::default()` has no `Hdr` component (`Hdr` lives in `bevy::render::view`), so `view.hdr = false`, the ViewTarget is `Rgba8UnormSrgb`, and `TONEMAP_IN_SHADER` stays active. **Adding `Hdr` breaks the sRGB round-trip** (float HDR target + suppressed in-shader tonemap).

**Frozen-contract pins for the camera (state in the example + docs):** `Camera2d` + `Tonemapping::None` (guaranteed by Core2dPlugin; the example may insert it explicitly to make the invariant local) + **no `Hdr`**.

### 3.4 Why it is correct for transparency (frozen)

For α<1, rive's premultiplied RGB must composite as `dst·(1-α) + src` — exactly `PREMULTIPLIED_ALPHA_BLENDING` (`color.src_factor=One`, `dst_factor=OneMinusSrcAlpha`). Straight `ALPHA_BLENDING` (plain `Sprite`) would double-multiply and darken edges. So `RiveMaterial` is the only correct and the frozen choice.

> **Premultiply-space precision note (frozen behavior, accepted):** rive premultiplies in **sRGB-encoded** space (verified intent: bytes are `encode(rgb)·alpha`), while the GPU sRGB-decodes RGB but **not** alpha, and `PREMULTIPLIED_ALPHA_BLENDING` operates in the render-target blend space. For α<1 over a non-black background this is therefore *technically approximate* (possible slight edge darkening/halo). This is moot for the opaque reference (α=1 ⇒ exact). It is an **accepted, frozen behavior shared identically with M1b** (M1b's VkImage carries the same bytes). No Bevy-side change; documented so M1b does not "discover" it.

### 3.5 The WGSL (`crates/bevy-rive/src/rive_material.wgsl`)

Material bind group is index **2** for Material2d (verified `MATERIAL_2D_BIND_GROUP_INDEX = 2`, material.rs:55; the `AsBindGroup` derive maps the struct's `@binding(0)/(1)` under `@group(2)`).

```wgsl
#import bevy_sprite::mesh2d_vertex_output::VertexOutput
#import bevy_sprite::mesh2d_view_bindings::view
#ifdef TONEMAP_IN_SHADER
#import bevy_core_pipeline::tonemapping
#endif

@group(2) @binding(0) var rive_texture: texture_2d<f32>;
@group(2) @binding(1) var rive_sampler: sampler;

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    // Sampler sRGB->linear decodes RGB. Bytes are already premultiplied; do NOT re-premultiply.
    var color = textureSample(rive_texture, rive_sampler, in.uv);
#ifdef TONEMAP_IN_SHADER
    // ACTIVE every frame for a default Camera2d; identity under Tonemapping::None + default grading.
    color = tonemapping::tone_mapping(color, view.color_grading);
#endif
    return color;
}
```

### 3.6 Staged fallback (flagged)

`Sprite::from_image(handle)` is byte-identical for the opaque clear (α=1 ⇒ straight==premultiplied), ~3 lines, useful to land the opaque match before wiring the material. But the public API is FROZEN ⇒ **land `RiveMaterial` from the start** so M1b reuses it verbatim. The plugin registers `Material2dPlugin::<RiveMaterial>` unconditionally; the example chooses the display.

---

## 4. Orientation contract (FROZEN) — **no flip anywhere, upright by construction**

- Rive readback is **top-down**: row 0 = TOP. The shim already flipped to upright (octopus confirmed upright in M0). "Readback is canonical upright" is the frozen invariant.
- A Bevy `Image` stores rows top-to-bottom; 2D UV convention samples `uv.y=0` at the top of the quad. **Verified UV pairing** (`bevy_mesh-0.18.1/src/primitives/dim2.rs`, `Rectangle`): top vertices (world-Y top) carry `UV.y=0`. So rive row 0 → Image data row 0 → `UV.y=0` → screen top ⇒ **upright**.
- Therefore `flip_x = false`, `flip_y = false`; default `Rectangle` UVs (no V-flip). The `Sprite` fallback is also upright at `flip_y=false` (default `uv_offset_scale`).
- **Any future inversion is fixed in the shim, never in Bevy** (keep "readback canonical upright"). Assert/document this.

---

## 5. File-by-file plan

```
crates/bevy-rive/
├── Cargo.toml
├── src/
│   ├── lib.rs                # plugin, asset, loader, components, material, resources, systems, make_rive_image
│   └── rive_material.wgsl    # embedded fragment shader (§3.5)
examples/
└── sprite_riv.rs             # registered on bevy-rive via [[example]]
```

### 5.1 `crates/bevy-rive/Cargo.toml`

> The **frozen `RiveMaterial` needs `bevy_sprite_render` in the LIB** (not just dev-deps) — corrected from the draft. The lib therefore cannot be "asset+image only"; it needs the render-side features whenever the material is compiled. Recommended: a default-on `render` feature gating the material + its deps. `wgpu-types` is pinned exact (`=27.0.1`) to avoid the `TextureFormat` type-identity hazard (§2.4).

```toml
[package]
name = "bevy-rive"
version = "0.0.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
description = "Bevy plugin for the native Rive Renderer (offscreen render + CPU-copy bridge, M1a)."
publish = false

[features]
default = ["render"]
# `render` pulls the Material2d display surface (RiveMaterial + embedded WGSL) AND the
# crates that define Material2d/AlphaMode2d/MeshVertexBufferLayoutRef. The frozen RiveMaterial
# lives behind this; without it the crate still defines asset/loader/components + fills the Image.
render = [
    "bevy/bevy_render",
    "bevy/bevy_core_pipeline",
    "bevy/bevy_sprite",
    "bevy/bevy_sprite_render",
    "bevy/bevy_mesh",      # MeshVertexBufferLayoutRef, Mesh2d
    "bevy/bevy_shader",    # ShaderRef
]

[dependencies]
rive-renderer = { path = "../rive-renderer" }
# Base: asset + image + the ECS/app/time stack come unconditionally.
bevy = { version = "0.18.1", default-features = false, features = ["bevy_asset", "bevy_image"] }
# Construct Image without bevy_render. EXACT-pin to the 27.0.1 bevy uses (type-identity).
wgpu-types = "=27.0.1"

[dev-dependencies]
bevy = { version = "0.18.1", default-features = false, features = [
    "bevy_asset", "bevy_image",
    "bevy_window", "bevy_winit", "x11", "wayland",
    "bevy_render", "bevy_core_pipeline",
    "bevy_sprite", "bevy_sprite_render", "bevy_mesh", "bevy_shader",
    "png", "multi_threaded", "default_font",
] }
image = "0.25"

[[example]]
name = "sprite_riv"
path = "../../examples/sprite_riv.rs"

[lints]
workspace = true
```

> Verify the exact bevy feature flag names (`bevy_mesh`, `bevy_shader`) resolve in `bevy_internal-0.18.1`'s `Cargo.toml` before committing; `bevy_render`/`bevy_core_pipeline`/`bevy_sprite_render` are confirmed. If `bevy_mesh`/`bevy_shader` are transitively enabled by `bevy_sprite_render`/`bevy_render` they may be omittable — keep them explicit to be safe.

### 5.2 `examples/sprite_riv.rs` — camera + quad + capture-and-exit

> Corrected imports (`bevy::sprite_render::*`, `bevy::mesh::Mesh2d` — both via prelude for `Mesh2d`/`MeshMaterial2d`, explicit for `RiveMaterial`'s sibling types). `Camera2d` + explicit `Tonemapping::None` + `Msaa::Off`. The premultiplied-alpha inspection capture uses **`Screenshot::image(target.image.clone())`** (the raw offscreen Rive texture) — NOT the window — because a window screenshot is the *composited* swapchain (post-blend over the clear) and cannot show straight premultiplied edges (soundness critique, severity minor; verified `Screenshot::image` exists, `screenshot.rs:98`).

```rust
use bevy::prelude::*;
use bevy::math::primitives::Rectangle;
use bevy::core_pipeline::tonemapping::Tonemapping;
use bevy::render::view::Msaa;
use bevy::render::view::screenshot::{save_to_disk, Screenshot, ScreenshotCaptured};
use bevy::sprite_render::{Material2d, Material2dPlugin}; // (Plugin registered inside RivePlugin)
use bevy::winit::WinitSettings;
use bevy_rive::{RiveAnimation, RiveFile, RiveMaterial, RivePlugin, RiveTarget};

#[derive(Resource)]
struct Cfg { riv: String, w: u32, h: u32, capture: Option<String>, warmup: u32 }

#[derive(Resource, Default)]
struct CaptureState { ready: u32, requested: bool, saved: bool }

#[derive(Component)]
struct RiveEntity;

fn main() {
    let riv = std::env::var("RIVE_RIV").unwrap_or_else(|_| "assets/octopus_loop.riv".into());
    let capture = std::env::var("RIVE_CAPTURE").ok();
    let warmup = std::env::var("RIVE_CAPTURE_FRAMES").ok()
        .and_then(|s| s.parse().ok()).unwrap_or(3);

    let mut app = App::new();
    app.add_plugins(DefaultPlugins)
        .add_plugins(RivePlugin)
        .insert_resource(WinitSettings::continuous())   // WSLg focus is flaky
        .insert_resource(Cfg { riv, w: 512, h: 512, capture, warmup })
        .init_resource::<CaptureState>()
        .add_systems(Startup, setup)
        .add_systems(Update, (attach_quad, drive_capture, do_exit).chain());
    app.run();
}

fn setup(mut commands: Commands, asset_server: Res<AssetServer>, cfg: Res<Cfg>) {
    // Frozen camera pins: Tonemapping::None + Msaa::Off (no Hdr).
    commands.spawn((Camera2d::default(), Tonemapping::None, Msaa::Off));

    let handle: Handle<RiveFile> = asset_server.load(cfg.riv.clone());
    commands.spawn((
        RiveAnimation::new(handle),
        RiveTarget { width: cfg.w, height: cfg.h, image: Handle::default() },
        RiveEntity,
    ));
    // attach_quad builds the textured quad once RiveTarget.image is written back.
}

// Once RiveTarget.image is real, spawn the textured quad ONCE.
fn attach_quad(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<RiveMaterial>>,
    q: Query<&RiveTarget, (With<RiveEntity>, Changed<RiveTarget>)>,
    cfg: Res<Cfg>,
    mut done: Local<bool>,
) {
    if *done { return; }
    let Ok(target) = q.single() else { return };
    if target.image == Handle::default() { return; }
    let mesh = meshes.add(Rectangle::new(cfg.w as f32, cfg.h as f32));
    let material = materials.add(RiveMaterial { texture: target.image.clone() });
    commands.spawn((Mesh2d(mesh), MeshMaterial2d(material), Transform::IDENTITY));
    *done = true;
}

fn drive_capture(
    mut commands: Commands,
    mut state: ResMut<CaptureState>,
    cfg: Res<Cfg>,
    q: Query<&RiveTarget, With<RiveEntity>>,
) {
    let Some(path) = &cfg.capture else { return };
    let Ok(target) = q.single() else { return };
    if target.image != Handle::default() { state.ready += 1; }
    if state.ready >= cfg.warmup && !state.requested {
        let p = path.clone();
        // (a) Quick visual: composited window (RGB; save_to_disk uses try_into_dynamic).
        commands.spawn(Screenshot::primary_window()).observe(save_to_disk(p.clone()));
        // (b) Premultiplied-alpha inspection: the RAW offscreen Rive texture (NOT the window).
        let img_handle = target.image.clone();
        let pa = format!("{p}.rgba.png");
        commands.spawn(Screenshot::image(img_handle))
            .observe(move |ev: On<ScreenshotCaptured>, mut st: ResMut<CaptureState>| {
                let img = &ev.image;
                if let Some(data) = img.data.as_ref() {
                    let _ = image::save_buffer(
                        &pa, data, img.width(), img.height(), image::ColorType::Rgba8);
                }
                st.saved = true;
            });
        state.requested = true;
    }
}

fn do_exit(state: Res<CaptureState>, cfg: Res<Cfg>, mut exit: MessageWriter<AppExit>) {
    if cfg.capture.is_some() && state.saved {
        exit.write(AppExit::Success); // 0.18 API: MessageWriter::write, not EventWriter::send.
    }
}
```

> `MeshMaterial2d` / `Mesh2d` come from `bevy::prelude::*`. `On<ScreenshotCaptured>` is the 0.18 observer event param (verified `save_to_disk: FnMut(On<ScreenshotCaptured>)`, screenshot.rs:129). `ScreenshotCaptured.image: Image` with `data: Option<Vec<u8>>` (screenshot.rs:47, image.rs:585). Capture (b) is the **frozen-contract-faithful** premultiplied output; capture (a) is the composited window for the opaque visual compare only. Exit only after `saved=true` guarantees the file is flushed.

### 5.3 Optional 3D path (flagged, `RIVE_3D=1`)

`Camera3d` at `Transform::from_xyz(0,0,2).looking_at(Vec3::ZERO, Vec3::Y)`; `Mesh3d(meshes.add(Rectangle::new(1.6,0.9)))`; `MeshMaterial3d(StandardMaterial { base_color_texture: Some(image), alpha_mode: AlphaMode::Premultiplied, unlit: true, ..default() })`. `AlphaMode::Premultiplied` exists in 3D (unlike `AlphaMode2d`); `unlit:true` samples the `Rgba8UnormSrgb` texture directly — same frozen Image. Needs `bevy_pbr` in dev-deps. Set `Tonemapping::None` + `Msaa::Off` on the 3D camera for the same color/diff guarantees.

---

## 6. Verification plan

### 6.1 Pass criteria (composited ⇒ tolerance, not byte-exact at edges)

With `Msaa::Off` + nearest sampler + 1:1 sizing, the **interior** is byte-exact (sRGB round-trip identity); only sampler/rounding and silhouette pixels diverge. Define a pass as:

1. **Orientation:** octopus upright (no V-flip) — visual + a top-half-mass/centroid check vs reference.
2. **Dominant color:** captured-RGB mean/histogram-peak matches the reference within per-channel mean Δ ≤ ~8/255.
3. **Coverage:** count of non-background pixels (differing from `0x303030` by > a small threshold) within ~±10% of the reference, bounding-box IoU ≳ 0.8.

`Msaa::Off` is **required** (not optional) for the capture/diff path; with default `Sample4` every silhouette pixel differs.

### 6.2 Linux (WSLg)

```bash
WGPU_BACKEND=vulkan RIVE_RIV=assets/octopus_loop.riv \
RIVE_CAPTURE=cap_octopus.png RIVE_CAPTURE_FRAMES=3 \
  cargo run --example sprite_riv
```

Dozen (Vulkan-on-D3D12) is selected by `WGPU_BACKEND=vulkan`; `WinitSettings::continuous()` keeps frames flowing. The app self-captures `cap_octopus.png` (composited window, RGB) + `cap_octopus.png.rgba.png` (raw offscreen Rive texture, RGBA premultiplied) and exits `AppExit::Success`.

### 6.3 Windows (relay)

```bash
./scripts/sync_to_windows.sh
cmd.exe /c "set WGPU_BACKEND=vulkan&& set RIVE_RIV=assets\octopus_loop.riv&& set RIVE_CAPTURE=cap_octopus.png&& scripts\win.cmd run --example sprite_riv"
```

`win.cmd` sets the VS/clang-cl toolchain (and defaults `WGPU_BACKEND=vulkan`); `WGPU_BACKEND=dx12` is the fallback if Vulkan is flaky. Identical example code; only the backend env differs.

### 6.4 Comparison harness

A dev tool or `cargo test` behind an env flag loads `cap_*.png` + the reference and computes the three §6.1 metrics with the stated tolerances, printing pass/fail. Keep it **out of the frozen lib** (example/test-only). For the opaque clear, comparing `cap_octopus.png` (window RGB) vs `out_octopus.png` (opaque RGB) is valid. The `.rgba.png` (raw offscreen) is for eyeballing premultiplied-alpha edges (qualitative; no opaque reference exists for transparent backgrounds).

---

## 7. Flagged decisions

1. **Asset payload `Arc<[u8]>`** (cheap clone for fan-out; matches `bevy_audio`).
2. **Two components, not a bundle** (`#[require(RiveTarget)]` gives bundle ergonomics; input/output separable).
3. **Selector enums `#[non_exhaustive]` + reserved `speed`** (named selection/scrubbing additive later).
4. **`RiveTarget.image == default` ⇒ plugin allocates** (plugin owns the frozen format; user may supply own handle).
5. **Display = custom `RiveMaterial` (Material2d + `PREMULTIPLIED_ALPHA_BLENDING`)** — only transparency-correct + M1b-reusable path; `Sprite` is opaque-only fallback.
6. **`RIVE_TEXTURE_FORMAT = Rgba8UnormSrgb` public**, documented as the plugin's allocation format (consumers read format off the Image, not the const).
7. **No CPU unpremultiply; bytes stay premultiplied sRGB** (M1b parity; `unpremultiply_rgba8` unused).
8. **`Context` lazy (`Option`) on first instantiate** (infallible `build()`; main-thread; failure logs + no-ops).
9. **Four chained NonSend `Update` systems** (clear `Changed`/load gating + apply-deferred barriers).
10. **`File` not stored** (derived-then-dropped; Rc graph keeps file data alive).
11. **`render` cargo feature (default on)** gates the material + render-side deps; the frozen `RiveMaterial` requires `bevy_sprite_render` **in the lib**.
12. **Backend via `WGPU_BACKEND` env (script-set)** — Linux/WSLg `vulkan`; Windows `vulkan`/`dx12`.
13. **Dual capture:** `save_to_disk(primary_window)` (composited RGB visual) + `Screenshot::image(target.image)` RGBA (raw offscreen premultiplied, contract-faithful). Exit after observer sets `saved`.
14. **`Msaa::Off` REQUIRED for diff** + nearest sampler + 1:1 sizing; tolerance metrics (§6.1) for residual edges.
15. **3D path behind `RIVE_3D=1`** (`StandardMaterial { unlit, alpha_mode: Premultiplied }`; needs `bevy_pbr`).
16. **No flip anywhere** (§4); inversions fixed in the shim.
17. **Camera pins explicit in the example:** `Tonemapping::None` (don't rely on `Camera2d::default()` "forcing" it — it comes from `Core2dPlugin`) + **no `Hdr`** (HDR breaks the sRGB round-trip).
18. **`wgpu-types = "=27.0.1"` exact-pin** in the lib (`TextureFormat` type-identity with `bevy_image`); `cargo tree -d` CI guard.

---

## Open flags for the human

1. **`render` feature gating vs always-on.** The frozen `RiveMaterial` forces `bevy_sprite_render` into the lib whenever the material compiles, so the "minimal asset+image-only lib" is impossible *with* the material. Options: (a) default-on `render` feature as specced (lib is heavy by default, slim only with `--no-default-features`); (b) drop the feature, render deps always on (simpler manifest). Pick one.
2. **Exact bevy feature-flag names `bevy_mesh` / `bevy_shader`.** Confirmed needed for `MeshVertexBufferLayoutRef` / `ShaderRef`; verify they are real `bevy` features in `bevy_internal-0.18.1/Cargo.toml` (vs being pulled transitively by `bevy_render`/`bevy_sprite_render`) before committing the manifest.
3. **`RiveTarget.image` vs `RiveMaterial.texture` naming asymmetry** — frozen permanently. Keep as-is (specced) or rename one for symmetry before freeze. Cosmetic only.
4. **Cleanup mechanism:** `Entities::contains` + `retain` (specced, dep-light) vs `RemovedComponents<RiveAnimation>` (cleaner). Both main-thread-safe; choose.
5. **Image allocation ownership:** two-phase "plugin allocates + `attach_quad` patches the quad" (specced) vs example pre-creates the Image and passes the handle into `RiveTarget.image` (no `attach_quad`). The former keeps format-ownership in the plugin; choose for the example.
6. **Whether to also wire the optional 3D path now** (`RIVE_3D=1`, needs `bevy_pbr` dev-dep) or defer it.
7. **Premultiply-space approximation for α<1** (§3.4): accept the frozen sRGB-space-premultiply behavior (shared with M1b) as-is, or escalate to the shim team to switch to linear-space premultiply before the contract freezes. Opaque reference is unaffected either way.