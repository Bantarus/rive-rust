# M-SCALE design — atlas batching (one begin/flush over an atlas)

> **Status: DESIGN (2026-06-02).** Validated lever, not yet built. Supersedes the
> "Tier B" sketch in `docs/engine-plugin-rive-spec.md` §11 with a measured,
> code-grounded plan. Authored from a 6-dimension design panel + 3 adversarial
> critiques (workflow `wf_d6080f5f-402`); every load-bearing claim is cited to
> committed code.

## 1. Why — the measured lever

Rendering N **independent** animated rive faces is O(N) main-thread, and the
per-phase split (native RTX 4090, instrumented at
[zero_copy.rs:1313](../../crates/bevy-rive/src/zero_copy.rs#L1313)/`frame_advance_us`,
`frame_cpu_us`, `frame_blit_us`; artifact `docs/perf/mscale_phase_split_raw.txt`) is:

| phase | share @ N=256 | parallelizable? |
|---|--:|---|
| **record** (rive tessellation + flush) | **76–92%** | **No** — one shared `RenderContext` + one wgpu command buffer (serial by design) |
| advance (state-machine tick) | ~5% (≤23% noisy) | yes, but too small to matter |
| blit (un-premult pass) | ~1% | n/a |

The cost is **per-flush fixed overhead × N** (per-instance `record` inflates 20 µs →
55 µs from N=32→256 while the GPU stays ~30 µs/inst). A **spike** —
`Context::record_external_frame_batched`, one `beginFrame` / N `draw`s / one `flush`,
gated behind `RIVE_BATCH` — cut record CPU **4.4–4.6×** (N=256: 28 ms → 6 ms) and
lifted N=256 from ~46 → **109 fps**, where the GPU fill (~9.35 ms measured via
`RIVE_BLOCKING`) becomes the new floor. The spike drew all N **overlapping** into one
target (CPU-faithful, not shippable).

**This doc designs the shippable form:** each artboard drawn into its **own tile** of
a shared **atlas**, in one begin/flush; each face samples its tile. Threading
(B.2/B.3) stays demoted — record is serial and advance is ~5%.

## 2. Architecture at a glance

```
MAIN world (Update)                          RENDER world (RiveFillNode::run)
─────────────────────                        ────────────────────────────────
allocate_display_images:                     pass 1: advance every ACTIVE instance
  - main-world AtlasPacker assigns a          pass 2: per atlas PAGE —
    (page, slot) -> uv_rect for each            begin_atlas_frame(page target)
    atlas-opted face                            for tile in page: draw_viewport(tile)   ← one flush
  - writes RiveSurface{image=page handle,       record   (rive draws all tiles -> page)
    uv_rect, atlas_size} on the entity        pass 3: ONE un-premult blit per page
  (dedicated texture if atlas=None)             (UNPREMULT_WGSL, dst-pixel==src-pixel)
        │                                              │
        └── Extract (uv_rect on ExtractedRive) ────────┘
Consumer reads RiveSurface: 3D -> StandardMaterial.uv_transform; 2D -> Sprite.rect
A plugin system re-syncs uv_transform/Sprite.rect on Changed<RiveSurface>.
```

Key resolution of the **C3 data-flow break**: tile *placement* (the `uv_rect`) is
computed **main-world** in `allocate_display_images`
([zero_copy.rs:883](../../crates/bevy-rive/src/zero_copy.rs#L883), the sole seam
writer) — at the same point and frame `image` is written — because the packer needs
only sizes (`RiveTarget.width/height`), not GPU state. The render world *reads*
`uv_rect` off `ExtractedRive` and the atlas page texture's grid layout is deterministic
from `(bucket, page, slot)`, so main and render agree without a render→main channel.

## 3. The layers

### 3.1 Shim — `rive_artboard_draw_viewport` (per-tile draw + clip)

New C entry next to `rive_artboard_draw`
([rive_shim.cpp:617](../../crates/rive-renderer-sys/shim/rive_shim.cpp#L617)):

```c
RiveStatus rive_artboard_draw_viewport(RiveArtboard* artboard, RiveRenderContext* ctx,
                                       float x, float y, float w, float h) {
    // ... null/positive checks ...
    rive::RiveRenderer* r = ctx->currentRenderer;
    const rive::AABB tile(x, y, x + w, y + h);
    const rive::Mat2D m = rive::computeAlignment(rive::Fit::contain,
        rive::Alignment::center, tile, artboard->artboard->bounds());
    r->save();
    // CLIP FIRST, at IDENTITY: clipRectImpl captures clipRectMatrix = stack.back().matrix
    // (rive_renderer.cpp:281). Before transform(m) that is identity -> the rect stays in
    // ATLAS-PIXEL space, independent of the artboard's own overflow.
    rive::rcp<rive::RenderPath> clip = ctx->renderContext->makeRenderPath(tile);
    r->clipPath(clip.get());     // makeRenderPath(AABB) -> IsAABB -> clipRectImpl FAST PATH
    r->transform(m);
    artboard->artboard->draw(r);
    r->restore();
    return RIVE_OK;
}
```

**The clip is MANDATORY, not optional** (C2-verified): rive's only draw-time bounds
cull is `isOutsideCurrentFrame` ([render_context.cpp:410](../../vendor/rive-runtime/renderer/src/render_context.cpp#L410)),
tested against the **whole atlas**, not the tile. Overflow content (strokes, feather,
overflow shapes) reaching a neighbor tile is **not** culled — only the per-fragment
`clipRect` coverage gate stops the write. `Fit::contain` bounds only the nominal
`bounds()` AABB, so it does **not** prevent bleed.

The clip is **near-free** and verified to **not** touch the PLS/coverage machinery
M2c tuned: `makeRenderPath(AABB)` → `IsAABB` ([rive_renderer.cpp:16](../../vendor/rive-runtime/renderer/src/rive_renderer.cpp#L16))
→ `clipRectImpl` writes one `ClipRectInverseMatrix` uniform, consumed per-fragment as
`coverage = min(coverage, clipRectCoverage)` ([atomic_draw.glsl:442](../../vendor/rive-runtime/renderer/src/shaders/atomic_draw.glsl)) —
**no mask draw, no clip-ID, no clip-buffer**. `frameSupportsClipRects` is true for all
our PLS modes (never msaa). The artboard's own self-clip composes on the fast path
(Fit::contain is pure scale+translate → no skew-reject → no `clipPathImpl` fallback).

**C2 correction — the AA gutter:** the `clipRect` edge is anti-aliased over a ~1px band
in atlas pixels; rive **writes fractional coverage up to ~0.5px outside** the nominal
rect ([common.glsl:344-370](../../vendor/rive-runtime/renderer/src/shaders/common.glsl), `_fragCoord = _pos.xy`).
Edge-to-edge tiles therefore corrupt each other's boundary pixels. **The allocator MUST
leave a ≥2px transparent gutter between tiles** (or inset the clip rect by ~0.5px) — a
*writer-side* contract, separate from the sampler-side half-texel inset.

Wrapper: replace the spike's `record_external_frame_batched` with a **builder guard**
([rive-renderer/src/lib.rs](../../crates/rive-renderer/src/lib.rs)):

```rust
let mut frame = unsafe { ctx.begin_atlas_frame(page_target, clear, record)? };
for tile in page.active_tiles() { frame.draw_viewport(tile.artboard, tile.rect)?; }
unsafe { frame.record()? };   // Drop without record() still flushes, leaving ctx clean
```

A builder (not a `&[(&Artboard,Rect)]` slice) lets the node interleave per-tile lookup
between draws and guarantees the in-progress frame is always closed.

### 3.2 Atlas resource — per-LOD-bucket paged grids (C1-corrected)

A small set of **fixed-tile-size buckets** (e.g. 64²/128²/256²), each a list of
**pages**; a page is one wgpu texture wrapped **once** as a rive `RenderTarget` (mirror
[build_instance:1570](../../crates/bevy-rive/src/zero_copy.rs#L1570) but allocated per
page, not per face). A page is a fixed grid → slot alloc is an O(1) bitset/free-list,
**no shelf packer, no fragmentation, no repack-invalidates-everyone**.

**C1 corrections (hard):**
- **Cap tiles-per-flush.** rive auto-splits a flush that exceeds its internal per-frame
  draw/path limits — silently undoing the record win. Size a page so its tile count
  stays **under rive's per-flush limit** (vendor check the exact cap; conservative
  start ≤ 64–256 tiles/page) → multiple pages → O(pages) flushes, still ≪ O(faces).
- **Budget the PLS coverage backing.** rive's coverage/atlas buffers are sized to the
  **page dimensions**, independent of how many tiles are filled → a big page costs
  coverage VRAM even when sparse. Page size **balances flush-count vs coverage-VRAM**;
  do **not** default to one giant 8192² atlas. Clamp page dim to
  `min(4096, limits().max_texture_dimension_2d)`.
- Growth = add a sibling page (LAYERS_PER_PAGE=1; defer texture-array pages until a
  vendor check confirms `makeRenderTarget` accepts `baseArrayLayer`).

Memory: N=256 same-size 256² faces in one 4096² page = 1 page = 64 MiB **and 1 rive
target / 1 flush** (was 256 VkImages / 256 flushes).

### 3.3 Seam — opt-in `RiveTarget.atlas` + write-back `RiveSurface` (C3-corrected)

`RiveTarget` is `#[non_exhaustive]` with an "additive fields" promise
([lib.rs:345](../../crates/bevy-rive/src/lib.rs#L345)). Atlas is **opt-in**, default off
→ **the default path is provably byte-identical** (both existing consumers untouched):

```rust
// REQUEST (opt-in): atlas = None -> today's dedicated per-face texture (default).
pub struct RiveTarget { pub width: u32, pub height: u32, pub image: Handle<Image>,
                        pub atlas: Option<RiveAtlasKey> /* NEW, default None */ }
impl RiveTarget { pub fn atlased(w,h,key) -> Self { ... } }

// OUTPUT (plugin writes back; the canonical thing atlas-aware consumers read):
#[derive(Component)]
pub struct RiveSurface { pub image: Handle<Image>, pub uv_rect: Rect, pub atlas_size: UVec2 }
```

`RiveSurface` (a separate component, **not** repurposing `RiveTarget.image`) avoids the
"two handles must stay equal" footgun and the silent full-UV regression. For
`atlas=None`, `RiveSurface.image == RiveTarget.image` (dedicated) and `uv_rect = full`.

**C3 fix — the packer is MAIN-WORLD.** Move tile placement into
`allocate_display_images` ([:883](../../crates/bevy-rive/src/zero_copy.rs#L883)), the
sole main-world seam writer: a `RiveAtlasMap` resource assigns `(page, slot) → uv_rect`
and writes `RiveSurface` in the **same system, same frame** `image` is written, **before
any consumer reads**. This is feasible because placement needs only `width/height`
(already main-world). The render world adds `uv_rect` to
[`ExtractedRive`:848](../../crates/bevy-rive/src/zero_copy.rs#L848) and reads it; the page
texture's grid is deterministic from `(bucket,page,slot)`, so the render-world texture
and main-world `uv_rect` agree with no render→main channel.

### 3.4 Display — one un-premult pass per page (UNPREMULT_WGSL unchanged)

rive fills one **premultiplied `Rgba8Unorm`** atlas page; one fullscreen pass
un-premults it into a straight-alpha **`Rgba8UnormSrgb` display page**. The existing
[`UNPREMULT_WGSL`](../../crates/bevy-rive/src/zero_copy.rs#L484) (`textureLoad(src,
vec2<i32>(pos.xy),0)`) needs **zero change**: shared and display pages share dimensions
and tile layout → **dst-pixel == src-pixel** for every tile, no remap, one cached bind
group, one draw per page (blit stays O(pages), not O(N)). The §7 Option-B math is
per-pixel and layout-agnostic; gutter pixels (a==0) resolve to straight `(0,0,0,0)`.
Consumers sample the **display** page sub-rect.

### 3.5 Fill node — three passes

Restructure [`RiveFillNode::run`:1135](../../crates/bevy-rive/src/zero_copy.rs#L1135):
1. **advance** every active (non-culled) instance — clean loop, rayon-droppable later
   (but advance is ~5%, so this is structure, not a perf ask).
2. **per page:** `begin_atlas_frame` → N `draw_viewport(tile.rect)` → `record`. One flush
   per page. The watermark/`safe_frame` ([:1267](../../crates/bevy-rive/src/zero_copy.rs#L1267))
   collapses to one compute/frame; the single `add_signal_semaphore` is unchanged.
3. **per page:** one un-premult blit (§3.4).

**Clear semantics:** rive `beginFrame` hardcodes `LoadAction::clear` over the whole page
([:1115](../../crates/rive-renderer-sys/shim/rive_shim.cpp#L1115)). v1 **redraws every
active tile each frame** (whole-page clear is then correct). **Cull is handled at the
active-set level** (Tier A / `RiveActive` decides *which* faces are atlas-backed and
drawn), **not** by per-tile skip — an off-screen face simply isn't in the active set, so
its black-while-offscreen tile is invisible; re-entry redraws it. (A `LoadAction::preserve`
+ dirty-tile path is a deferred optimization, not v1.)

### 3.6 Consumer — `uv_transform` (3D) / `Sprite.rect` (2D), no custom material

C3-verified against the pinned 0.18.1:
- **3D (voxelith decals):** `StandardMaterial.uv_transform: Affine2` exists
  (`pbr_material.rs:787`) and the shader applies `uv = (uv_transform * vec3(uv,1)).xy`
  (`pbr_fragment.wgsl:126`). The decal is a `Rectangle` with full-[0,1] UVs
  ([rive_face.rs:163](../../../voxelith/src/rive_face.rs)) → **one added line**
  `uv_transform: RiveSampling::uv_transform(surface.uv_rect)`. **No custom material, no
  mesh-UV rebuild** (critical — `rive_stress.rs` reuses ONE shared quad mesh for all N;
  mesh remap would explode to N meshes).
- **2D (the `sprite_riv_zerocopy` example):** `Sprite.rect = RiveSampling::sprite_rect(
  uv_rect, atlas_size)` (normalized→pixels via a plugin helper).
- **MANDATORY re-sync system** (C3): the plugin ships a system that on
  `Changed<RiveSurface>` re-writes `uv_transform`/`Sprite.rect`. Without it, consumers
  that **latch the material once** ([rive_face.rs:122,`*done`](../../../voxelith/src/rive_face.rs))
  would strand a stale full-atlas UV on a one-frame-late `uv_rect` or a LOD repack →
  permanent mis-sample. (The main-world packer makes the first-frame `uv_rect` correct;
  the re-sync covers later LOD/repack changes.)
- Helpers (`RiveSurface`, `RiveSampling`, `RiveAtlasKey`) go in `bevy_rive::prelude`
  ([lib.rs:142](../../crates/bevy-rive/src/lib.rs#L142)) — `engine-plugin-rive`'s glob
  re-export carries them to voxelith automatically (verify under the feature-gated glob).

## 4. Composition with the rest of Tier B

- **B.1 cull (`RiveActive`):** decides the active set → only active faces hold tiles and
  get a `draw_viewport`; bounds page count + the batched draw list. Hook slot-free into
  the instance-retain at [:1652](../../crates/bevy-rive/src/zero_copy.rs#L1652).
- **B.5 per-LOD resolution:** a face's `width/height` picks its bucket; a LOD change is an
  O(1) free+alloc across buckets, no repack, no UV churn for others. `draw_viewport`
  takes arbitrary `(w,h)`, so variable tiles are first-class.
- **Tier A game-side pool:** caps live faces to K, so the atlas only ever holds K tiles
  (packer pressure bounded by the pool, not total NPCs). A pooled source keeps a stable
  slot → stable `uv_rect` across a pool generation.
- **Threading (B.2/B.3):** still demoted — record is serial; advance is ~5%.

## 5. Phasing (each phase independently measurable)

- **Phase 0 — DONE.** Spike validated the record lever (`RIVE_BATCH`, overlapping).
- **Phase 1 — shim + clip, no Bevy.** `rive_artboard_draw_viewport` + the `begin_atlas_frame`
  builder + a `rive-renderer` offscreen test: render 2–4 distinct artboards into one
  multi-tile target; assert **zero bleed** with an overflow fixture, and **re-measure
  record CPU WITH the clip** (C2 gate) vs the spike's clip-free number. *Needs a relay
  shim rebuild.*
- **Phase 2 — single-page atlas in bevy-rive.** One bucket, fixed grid, main-world
  packer, `RiveSurface` seam, 3-pass node, one un-premult pass. Relay gate: reproduce
  ~6 ms record / ~100 fps at N=256 **writing distinct tiles**, per-tile ≤1–2 LSB vs the
  M1a render, §9 magenta test per non-origin tile.
- **Phase 3 — scale + compose.** Multi-page + per-LOD buckets (B.5), tiles-per-flush cap
  + coverage budget (C1), `RiveActive` cull, the `Changed<RiveSurface>` re-sync system.
- **Phase 4 — consumer migration.** voxelith `uv_transform` one-liners; `engine-plugin-rive`
  prelude re-export; migration-zero proof (default path byte-identical); docs in both repos.

## 6. Risks (critique-corrected) & open vendor checks

| risk | mitigation | source |
|---|---|---|
| rive auto-splits an over-large flush → win lost (C1) | cap tiles/page under rive's per-flush limit; multi-page | C1 |
| PLS coverage VRAM scales with page dims (C1) | page-size budget; clamp dim; don't default giant | C1 |
| clipRect AA writes ~0.5px outside tile → edge corruption (C2) | **≥2px gutter** (writer-side) + sampler half-texel inset | C2, common.glsl:359 |
| clip cost on heavy-overflow content (overflow still tessellated) | measure clip-on vs clip-off; keep clip regardless | C2, render_context.cpp:410 |
| render-world rect can't reach main-world seam (C3) | **main-world packer** in `allocate_display_images` | C3 |
| single-shot-latched consumer strands stale UV (C3) | main-world first-frame correctness + mandatory re-sync system | C3, rive_face.rs:122 |

**Open vendor checks (rive-runtime):** (1) exact per-flush draw/path limit → the
tiles-per-page cap; (2) does `makeRenderTarget` accept `baseArrayLayer` → texture-array
pages; (3) coverage-buffer size formula vs page dims → the VRAM budget; (4) whether any
shipped fixture overflows its bounds enough to exercise the clip (quantifies the gutter).

## 7. Acceptance gates (roll-up)

1. **No-bleed:** 2 artboards (one overflowing) in a 2-tile target → the boundary column
   shows zero pixels from the neighbor (Phase 1).
2. **Record win holds WITH clip:** N=256 in one atlas reproduces ~6 ms record / ~100 fps,
   GPU-fill the floor (Phase 2).
3. **Per-tile correctness:** each tile ≤1–2 LSB vs the M1a render; §9 magenta test clean
   at a non-origin tile (straight-alpha, no fringe).
4. **Migration-zero:** unmodified rive-bevy-consumer + voxelith default path render
   byte-identical (atlas=None → dedicated texture, uv_rect=full).
5. **Atlas opt-in:** N atlas faces → shared `RiveSurface.image`, disjoint in-[0,1]
   `uv_rect`s, each tile samples its own face; LOD repack re-syncs `uv_transform`.
6. **Validation-clean** under the M2 core+sync layers; no atlas-space leak over
   spawn/despawn churn.
