# Architecture

This document describes how `rive-rust` is built: the offscreen-texture model, the
four-layer stack and its per-feature maintainability contract, the two rendering tiers,
the backend-tagged C ABI that lets new GPU backends land without ABI churn, and the
display contract every consumer relies on.

It is a conceptual overview. For *which Rive features are exposed today and what is
planned*, see the feature matrix in [`feature-support.md`](feature-support.md).

`rive-rust` drives the **native Rive Renderer** (the vendored C++/Vulkan PLS runtime),
not a reimplementation. Its job is to render a `.riv` artboard / state machine into a GPU
texture that an engine can display.

---

## The offscreen-texture model

The integration is deliberately small in concept: **rive renders into a texture, and the
host engine displays that texture.**

Every frame, the state machine is advanced and drawn into an offscreen surface. The
result is handed to Bevy as a `Handle<Image>` (the `RiveTarget.image`). From there it is
an ordinary texture: bind it to a 2D `Sprite`, or map it onto a 3D `StandardMaterial`.
There is no Rive-specific compositor, scene graph, or draw call in the host — just a
texture that changes each frame.

Two consequences follow, and they shape everything below:

- The host does not need to understand Rive's drawing model; it only needs a texture.
- *How* the texture gets filled is a swappable implementation detail. The two tiers below
  fill the same `Handle<Image>` two different ways, and a consumer's spawn/display code is
  identical either way.

---

## The four-layer stack

Every runtime-control feature (anything the host drives or reads — pointer input, data
binding, artboard selection, and so on) crosses the same four layers. Rendering and
playback features need no per-feature code: they are drawn automatically by
`advance()` + `draw()`.

| Layer | Where it lives | What it holds |
|-------|----------------|---------------|
| 1. C++ shim | `crates/rive-renderer-sys/shim/` | `extern "C"` entry points that call into the vendored rive runtime through its public API only. |
| 2. FFI declarations | `crates/rive-renderer-sys/src/lib.rs` | the raw `extern "C"` signatures, one banner section per feature. |
| 3. Safe wrapper | `crates/rive-renderer/src/<feature>.rs` | `Result`-based, typed methods over the FFI (e.g. `impl Artboard`). |
| 4. Bevy integration | `crates/bevy-rive/src/<feature>.rs` | a `Component` plus the system that drives it each frame. |

The vendored runtime under `vendor/rive-runtime` is a **pristine** submodule pinned to a
release tag. It is never patched; the shim only calls its public API. Keeping the runtime
unmodified is what lets it be updated by bumping the submodule rather than reconciling a
fork.

### The per-feature maintainability contract

A feature is *one cohesive unit at each layer*, so the codebase stays navigable as
coverage grows:

- **one shim translation unit** for the feature's C ABI,
- **one FFI banner section** declaring its raw signatures,
- **one safe-wrapper module** exposing typed, `Result`-returning methods,
- **one Bevy component + system** that owns its ECS surface.

Adding a feature means touching exactly those four places — register the new shim TU in
the build, declare the FFI under its banner, add the wrapper module, and add the
component. Worked examples already in the tree are **pointer input** and **view-model data
binding**: each is a self-contained vertical slice through all four layers. This is the
contract the project follows to reach broad Rive coverage without the integration becoming
a tangle. The current status of each slice is tracked in
[`feature-support.md`](feature-support.md).

---

## The two rendering tiers

Both tiers produce the *same* result — a straight-alpha, upright `RiveTarget.image` — and
share the entire frozen component API (`RiveFile`, `RiveAnimation`, `RiveTarget`, the
selectors). They differ only in how pixels reach the texture. Consumers select a tier by
Cargo feature; spawn and display code does not change.

### `floor` (default) — the CPU-copy bridge

The floor is the universal, loosely-coupled tier. rive renders the `.riv` to **its own**
offscreen Vulkan device; the plugin reads those pixels back to the CPU and copies them
into a Bevy `Image` each frame.

Because it never touches wgpu's device, the floor has no `ash`, no `wgpu-hal`, and no
exact-wgpu version pin — it depends only on a caret `bevy = "0.18"` plus `wgpu-types`. That
makes it a near-drop-in plugin and the portable fallback: it runs wherever the host runs,
including on a wgpu D3D12 backend. (Note: rive's offscreen device is self-managed Vulkan,
so a Vulkan ICD must still be present even when wgpu itself is on another backend.)

The cost is the per-frame CPU readback and copy. The floor is correctness- and
portability-first; the zero-copy tier removes that cost where the device can be shared.

### `zero_copy` (opt-in) — the shared-`VkImage` fast path

The zero-copy tier shares **one** GPU device with wgpu and renders the `.riv` *directly
into a wgpu-allocated `VkImage`*, with no per-frame CPU readback. The guiding principle is
**wgpu owns the device; rive borrows handles.**

- **Device sharing.** A `raw_vulkan_init` callback (installed before `DefaultPlugins`)
  appends the fragment-shader-interlock / raster-order extension to the device Bevy
  creates, so rive gets its clean PLS path. Bevy keeps owning the wgpu
  `VkInstance/VkPhysicalDevice/VkDevice/VkQueue`. The shim only **borrows** those handles —
  it never creates or destroys them.

- **Render-graph node.** rive's per-frame work runs inside a Bevy render-graph node
  (`RiveFillNode`), ordered before the main pass in the camera's sub-graph. The node
  advances each state machine and renders into a shared `Rgba8Unorm` texture, then runs a
  fullscreen pass into the display image (see the display contract below).

- **Non-blocking sync.** Rather than rive submitting and the CPU blocking on a fence, the
  shim **records rive's draws into wgpu's own already-open command buffer**. rive's work
  then rides wgpu's single per-frame submit, GPU-ordered ahead of the pass that samples the
  image — no CPU stall.

- **Timeline-semaphore watermark.** Without a blocking fence, rive may only recycle a
  pooled resource once its GPU work has actually completed. The tier signals its own Vulkan
  **timeline semaphore** with the frame number on each of wgpu's submits and reads it back
  to compute an *exact* GPU-completion watermark (`safeFrameNumber`). A fixed
  `current − ring_size` watermark is the fallback when timeline semaphores are unavailable,
  correct only while frames-in-flight stay within the ring.

- **Atlas batching.** For many faces, opt-in atlas batching packs faces into tiles on
  shared atlas pages (per-LOD tile buckets, a fixed grid per page, a small writer-side
  gutter so anti-aliased tile edges do not bleed). Each face's display image points at a
  sub-rect of the atlas, so many faces can flush together.

**Coupling cost.** Because it extracts raw handles via `as_hal`, the zero-copy tier is
ABI-locked to one exact set of `wgpu` / `wgpu-hal` / `ash` versions (the set Bevy 0.18.1
ships). A version skew is a Cargo **resolver error naming the versions** — by design,
never a silent `as_hal` corruption. Treat every Bevy bump as a deliberate re-validation,
not a routine `cargo update`. The tier also holds rive's `!Send` handles as a main-thread
`NonSend` resource and requires pipelined rendering to be disabled; the single-thread
invariant (not atomic refcounts) is what keeps the borrowed handles sound.

---

## The backend-tagged C ABI

The zero-copy ABI is shaped so that other GPU backends can be added **without ABI churn**.
Every external (device-sharing) entry point is *backend-tagged*, in a uniform three-call
shape:

- `create_<backend>_external(...)` — build a rive render context on a host-owned device
  the shim borrows.
- `wrap_<backend>_<resource>(...)` — wrap a host-allocated GPU image/texture as a rive
  render target (zero copy; the shim never allocates or frees it).
- `submit_external_<backend>(...)` — drive submission using *that backend's* model.

The signatures intentionally encode each backend's submission model rather than forcing a
lowest common denominator:

- **Vulkan** (shipping): rive records into a `VkCommandBuffer` (wgpu's own, in the
  non-blocking path), submitted to a `VkQueue`.
- **D3D12**: rive records into its own command list; the caller drives an
  `ID3D12CommandQueue` + `ID3D12Fence`/value (no external command buffer).
- **Metal**: rive's external command buffer is an `id<MTLCommandBuffer>`, which
  self-submits via `commit`.

The D3D12 and Metal entry points already exist as declared stubs, so the cross-backend ABI
surface is fixed now. Landing a backend is then an *implementation* behind a stable ABI —
a port of the Vulkan zero-copy design, not a new contract. The backend roadmap (which
backends ship, which are greenlit, which are out of scope) lives in the project
[`ROADMAP.md`](../ROADMAP.md).

---

## The display contract

Both tiers converge on **one** texture seam, so consumer code is tier-agnostic. The
`RiveTarget.image` is:

- **Format:** `Rgba8UnormSrgb` (sRGB-encoded `RGBA8`). Read the format off the allocated
  `Image` rather than hard-coding it; the *internal* working format is a per-tier choice.
- **Orientation:** **upright**, top-down rows. Orientation is corrected in exactly one
  place — the shim — so every downstream surface is upright by construction.
- **Alpha:** **straight** (non-premultiplied). rive's native output is premultiplied;
  **both tiers un-premultiply** before the seam — the floor on CPU readback, the zero-copy
  tier in a fullscreen un-premultiply + sRGB-decode pass.

The practical contract for a consumer: display `RiveTarget.image` with a `Sprite`, or with
a 3D `StandardMaterial` using `AlphaMode::Blend` — **not** `AlphaMode::Premultiplied`,
because the image is straight-alpha. This composites correctly in linear space for both
opaque and transparent content, identically across tiers.
