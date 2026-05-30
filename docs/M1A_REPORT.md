# Milestone 1a — report (Bevy plugin + CPU-copy bridge)

**Status: complete and verified on native Linux and native Windows.** A new
`bevy-rive` crate drives the native Rive Renderer to fill a Bevy `Image` every
frame (advance → render offscreen → read back → un-premultiply → copy into the
`Image`), displayed on a Bevy `Sprite`. This is the **CPU-copy floor** — the
universal fallback tier. No `wgpu-hal`, no `as_hal`, no shared `VkImage`, no
render-graph node: that is M1b (zero-copy). The `.riv`'s state machine **animates
continuously** (verified: the composite differs between frame 12 and 150 on both
OSes).

```
# Linux (WSLg) and Windows (relay), self-capturing verify + exit:
RIVE_CAPTURE=cap.png cargo run -p bevy-rive --example sprite_riv
cmd.exe /c "set RIVE_CAPTURE=cap.png&& E:\DEV\rive-rust\scripts\win.cmd run -p bevy-rive --example sprite_riv"
```

The plugin is pinned to **Bevy 0.18.1** (wgpu 27.0.1, ash 0.38, winit 0.30.13,
raw-window-handle 0.6.2 — all M1b-interop-ready). The full design spec (with
0.18.1-source-verified API citations) is at
[docs/design/M1A_DESIGN_SPEC.md](design/M1A_DESIGN_SPEC.md).

---

## 1. What builds and what runs

| Platform | GPU / Bevy backend | Offscreen `Image` | Sprite composite | Animates? |
| --- | --- | --- | --- | --- |
| **Linux** (WSLg) | Dozen / llvmpipe (Vulkan) | ✓ upright | ✓ upright | ✓ (frame 12 ≠ 150) |
| **Windows** | **native RTX 4090, Vulkan** | ✓ upright | ✓ upright | ✓ (frame 12 ≠ 150) |

`octopus_loop.riv` (the asymmetric orientation tell) renders **upright** with
correct colors — pixel-faithful to the M0/M1.0 references `out.png` /
`out_octopus.png` — and **animates continuously**: the composited sprite differs
between frame 12 and frame 150 on both OSes (the offscreen render does too,
confirming the state machine advances and the texture re-uploads to the sprite). On
Windows, Bevy's own renderer runs on the native 4090 via Vulkan (`AdapterInfo {
name: "NVIDIA GeForce RTX 4090", backend: Vulkan }`), and the rive offscreen device
runs the interlock PLS path.

The example self-captures two PNGs and exits with `AppExit::Success`: the
**composited window** (the sprite as displayed) and the **raw `Image`** (straight
RGBA). The window screenshot is gated on the display sprite actually rendering, so
the capture is GPU-speed-agnostic (a naive frame count raced the sprite on the fast
4090; the fix waits for it).

---

## 2. The frozen public API

What downstream tiers (M1b/M2/M3) reuse **verbatim** is the *type surface*, not the
fill mechanism:

| Frozen | Item |
| --- | --- |
| Asset + loader | `RiveFile { bytes: Arc<[u8]> }`, `RivLoader` (`.riv` → Bevy asset) |
| Components | `RiveAnimation { handle, artboard, state_machine, speed }` (`#[require(RiveTarget)]`, `#[non_exhaustive]`), `RiveTarget { width, height, image }` (`#[non_exhaustive]`) |
| Selectors | `ArtboardSelector`, `StateMachineSelector` (`#[non_exhaustive]` + `Default`; M1a honors only `Default`, named selection is an additive change later) |
| The seam | `RiveTarget.image: Handle<Image>` carrying the **upright** orientation |

Display is **user-side**: put the `Handle<Image>` on a Bevy `Sprite` (see §3 for
why a `Sprite`, not a custom material). The plugin ships no display material.

**Deliberately not frozen** (M1a implementation detail): the four systems and the
*way* the `Image` is filled (CPU readback + un-premultiply + copy,
`MAIN_WORLD | RENDER_WORLD` residency, the `Assets::get_mut` re-upload), the
**exact pixel format** (read it off the `Image`, never a constant — `RIVE_TEXTURE_FORMAT`
is `pub(crate)`), and the **alpha convention** (straight here; premultiplied for
M1b's zero-copy). M1b swaps the fill for a `RENDER_WORLD`-only shared `VkImage`
(`data: None`, no CPU copy) behind the same `Handle<Image>` seam.

`RivePlugin` registers the asset+loader, two `NonSend` resources, and four chained
`Update` systems. `DefaultPlugins` must precede it. The lib depends only on
`bevy_asset` + `bevy_image` (no render/sprite/winit crates) — it just fills an
`Image`.

---

## 3. Display, colorspace + orientation contract (the priority)

**Display = `Sprite`, and why it must be (the key M1a finding).** The CPU-copy
fill mutates the `Image` each frame via `Assets::get_mut`, which fires
`AssetEvent::Modified`, and Bevy 0.18.1's `GpuImage::prepare_asset` **recreates the
GPU texture** on every `Modified` ([gpu_image.rs:72-79]). Bevy's built-in `Sprite`
handles this — it invalidates its bind group on `Modified` ([sprite render
mod.rs:607-617]) and re-binds the new texture — so the sprite **animates**. A
custom `Material2d` (and a 3D `StandardMaterial`) instead **caches its bind group**
and never rebuilds when a referenced image changes, so it would freeze on the
first frame. (This was caught after the initial cut shipped a premultiplied
`RiveMaterial` that rendered a *static* sprite — verified: composite identical
across frames; replaced with `Sprite`, composite now differs frame-to-frame.)
A premultiplied material becomes viable in **M1b**, whose shared texture is
*stable* (rive writes in place — no per-frame recreation).

**Format / alpha (M1a):** the `Image` is `Rgba8UnormSrgb`, **straight** alpha. rive
outputs *premultiplied*; the plugin **un-premultiplies on readback**
(`unpremultiply_rgba8`, a no-op for opaque content) so that `Sprite`'s straight
`ALPHA_BLENDING` (`SrcAlpha, OneMinusSrcAlpha`) composites correctly in linear
space: sampler sRGB→linear gives `linear(c)`, blend gives `linear(c)·a + dst·(1-a)`
— the exact straight-over. This is correct for **both** opaque (matching the
references) **and** transparent content, sidestepping the gamma-vs-linear
premultiply mismatch that a premultiplied path would have. The cost is a per-frame
CPU un-premultiply pass (fine for the fallback tier; no perf claims — debug rive).

**Camera pins:** `Tonemapping::None` (else the camera's tonemap LUT alters rive's
already-final color), no `Hdr` (keeps the sRGB ViewTarget so the round-trip is an
identity), `Msaa::Off` (clean pixel diff). `Camera2d` already defaults
`Tonemapping::None` + `DebandDither::Disabled` (via `Core2dPlugin`); the example
sets them explicitly to make the invariant local.

**Why it matches the references exactly:** opaque clear (`0x303030ff`) → un-premult
is a no-op, and `srgb_to_linear(srgb_encode(c))·1 = linear(c)` → the
sample→linear→…→sRGB round-trip is a numerical identity.

**Orientation:** rive readback is top-down (upright — the shim already flips; the
octopus is the tell). A Bevy `Image` is top-down and a `Sprite` samples it upright,
so it is upright with **no flip anywhere**. Any future inversion is fixed in the
shim, never in Bevy.

**Alpha convention is per-tier (flagged, §6).** M1a is straight (un-premultiplied,
for `Sprite`); M1b's zero-copy texture keeps rive's *premultiplied* bytes (it
cannot un-premultiply zero-copy) and uses a premultiplied material on the stable
texture. The frozen seam is the `Handle<Image>` + orientation; the alpha convention
and display are per-tier (the user reads the tier's docs / the `Image` format).

---

## 4. The `rive-renderer` Rc-refactor (enabling ECS storage)

M0's wrapper bound every handle to `&Context` by lifetime, which cannot be stored
in a Bevy `'static` resource. The wrapper is refactored to **owned, `Rc`-shared**
handles (no lifetime params): `Context = Rc<ContextInner>`, and every
`RenderTarget`/`File`/`Artboard`/`StateMachine` holds an `Rc<ContextInner>`. The
`VkDevice` is destroyed only on the last drop, after every handle has run its own
native `*_destroy` (a manual `Drop` body runs before field drops). `StateMachine`
additionally holds `Rc<ArtboardInner>` so the native `rive::Scene` is destroyed
before the artboard instance it points at — the required orders hold **by
construction**, regardless of drop order. The handles are `!Send + !Sync` (Rc +
raw pointers), which is exactly what forces the `NonSend` placement below.

A safe-API soundness hole was found in review and fixed: `begin_frame`/`draw` now
reject a target/artboard built on a *different* `Context` (`Error::ContextMismatch`
via `Rc::ptr_eq`) instead of driving one `VkDevice`'s objects through another's —
undefined behavior previously reachable from entirely safe code. Validated by a
two-device test on the native 4090 (`#[ignore]`d on WSL2, whose Dozen ICD cannot
host two devices). `offscreen_png` (M0) still renders pixel-identical.

---

## 5. Architecture — the NonSend bridge

The native objects are `!Send`, so they live in two `NonSend` resources
(main-thread-pinned): `RiveContext` (the single lazily-created Vulkan context) and
`RiveInstances` (a `HashMap<Entity, RiveInstance>` of per-entity native state). The
public components are plain `Send + Sync` data; the `Entity` key links them to the
`!Send` native state (which cannot be a component). Four chained `Update` systems:
`instantiate` (build native state when the `.riv` loads), `advance_and_upload`
(per-frame core), `resize`, `cleanup` (drop on despawn, main-thread). M1b keeps
the components and replaces only what `advance_and_upload` does.

**Per-frame cost (no conclusions — rive libs are debug):** the M1a fill reads back,
un-premultiplies (a per-pixel CPU pass), and re-uploads a fresh GPU texture every
frame (Bevy recreates it on `Modified`; ~1 MiB at 512² per instance) — inherent to
the CPU-copy tier. As recorded in M1.0, the rive static libs are **debug builds**,
so per-frame timings are **not representative**; no perf claims are made.
Optimizing the copy/upload (in-place `write_texture`) and an optimized-rive build
are M2 work.

---

## 6. Decisions flagged for review (one-way doors before M1b)

1. **Alpha convention + display per tier (§3).** M1a un-premultiplies → straight →
   `Sprite` (correct for opaque *and* transparent). M1b keeps rive's premultiplied
   bytes (cannot un-premultiply zero-copy) → a premultiplied material on its stable
   texture. So the alpha convention and the display widget differ by tier. The
   frozen seam is the `Handle<Image>` + orientation; **confirm you're OK with
   display being user-side and per-tier** (vs the plugin shipping one display).
   The premultiplied material was *removed* from M1a because it froze the sprite
   (see §3); it returns in M1b where the stable texture makes it animate.
2. **`Image` pixel format vs M1b's `VkImage`.** rive's offscreen target is
   `VK_FORMAT_R8G8B8A8_UNORM`; M1b's zero-copy wgpu texture wrapping it is most
   naturally `Rgba8Unorm`, whereas M1a allocates `Rgba8UnormSrgb`. The format is
   intentionally **not** frozen (read off the `Image`) so M1b can choose. The M1b
   material's sampling/blend must match its chosen format + premultiplied bytes.
3. **Clear color is a fixed opaque `0x303030` const.** Matching the references. A
   configurable (incl. transparent) clear is a likely additive need; the components
   are `#[non_exhaustive]` so a field can be added without breakage.
4. **In-place texture upload (M2/M1b).** M1a recreates the GPU texture each frame
   (Bevy's `Assets::get_mut` path), which is why only `Sprite` (not a cached
   material) tracks it. An in-place `RenderQueue::write_texture` into a stable
   texture would let any material animate *and* cut the per-frame realloc — natural
   alongside M1b's stable shared texture.

---

## 7. Review & verification summary

A four-lens adversarial review (Rc-soundness, plugin-correctness, frozen-API
durability, colorspace) ran against the real code. Fixes applied:

- **M1** cross-context UB → identity check + `Error::ContextMismatch` (tested on HW).
- **M2** unbounded per-frame retry / log firehose / repeated device creation → tri-state
  terminal context init (`Uninit`/`Failed`/`Ready`) + a `failed` entity set (one log,
  no retry).
- **M3** transparency over-claim → honest docs + flagged decision (§6).
- **M4** `dt * speed` could feed NaN/negative/huge to native `advance()` → sanitized
  (`is_finite` + `max(0.0)`).
- **S1** `RIVE_TEXTURE_FORMAT` `pub` → `pub(crate)` (format not frozen).
- **S2** `RiveAnimation`/`RiveTarget` → `#[non_exhaustive]` + constructors (additive fields).
- **S3** freeze scope corrected (types frozen; systems/fill/format/alpha/data-residency not).

Then a **post-review verification finding** (the review checked static correctness;
multi-frame capture caught this): the premultiplied `RiveMaterial` rendered a
**static** sprite — a custom `Material2d` caches its bind group and never sees the
per-frame-recreated texture. Fixed by displaying via Bevy's `Sprite` (which
invalidates on `AssetEvent::Modified`) and **removing `RiveMaterial` + the WGSL +
the 3D path** from M1a (premultiplied material returns in M1b on its stable
texture). The Image is now un-premultiplied → straight, which also makes `Sprite`'s
straight blend correct for transparency (resolving the old M3 gamma-premult issue
for M1a). Lib deps dropped to `bevy_asset` + `bevy_image`.

The Rc drop-order core and orientation were confirmed sound. Remaining nits
(`cleanup` allocating a set each frame, `unpremultiply` ±1 LSB) are documented,
not blocking.

**Verified:** clippy `-D warnings` clean (workspace), `cargo test` green (rive-renderer
2 + doctests; cross-context test passes on the 4090), `cargo fmt --check` clean on all
M1a files, Linux + Windows captures match the references in color + orientation **and
animate** (frame 12 ≠ 150).

> Note: three pre-existing M0/M1.0 files (`build.rs`, `rive-renderer-sys/src/lib.rs`,
> `examples/offscreen_png.rs`) have fmt drift against rustfmt 1.94.1 that predates M1a;
> left untouched to keep this diff scoped. Worth a separate `cargo fmt --all` cleanup.

---

## 8. Notes carried into M1b

- The ECS surface (§2) is the frozen contract. M1b integrates via a Bevy Render
  Graph node + per-backend `as_hal` extraction, replacing only the internal fill.
- The `Handle<Image>` seam + the upright orientation carry over; the pixel format,
  alpha convention, and display widget are per-tier (decisions 1–2). M1b reintroduces
  a premultiplied material (now viable: its shared texture is stable) and chooses the
  format matching rive's `VkImage`.
- `out*.png` (M1.0) and the M1a captures are trustworthy cross-tier references for
  diffing M1b's zero-copy output — for **opaque** content (where straight ==
  premultiplied). Add a transparent reference when the alpha convention is settled.
- An in-place `RenderQueue::write_texture` (decision 4) is the natural M1b/M2 path:
  a stable texture lets cached materials animate and removes the per-frame realloc.
- `Context::as_raw()` / `RenderTarget::as_raw()` are the documented escape hatches
  M1b uses for wgpu/ash interop.
