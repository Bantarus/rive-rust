# Roadmap

A living, link-light view of where `rive-rust` is and where it is going. For the
detailed, per-feature status of Rive runtime features, see the feature matrix in
[`docs/feature-support.md`](docs/feature-support.md). Work items are tracked as GitHub
issues, organized by milestone labels (`M3a`, `M3b`) and area labels.

## Where we are

`rive-rust` provides Rust bindings and a Bevy plugin over Rive's native C++/Vulkan PLS
renderer (the real rive-runtime, not a reimplementation). It renders a `.riv`
artboard/state-machine into a wgpu texture (a Bevy `Handle<Image>`) you can show on a 2D
sprite or map onto a 3D material.

Two rendering tiers ship today, both validated on Vulkan:

- **floor** (default) — CPU-copy. Rive renders offscreen on its own device; pixels are
  read back and copied into a Bevy `Image`. Loosely coupled: no `ash` / `wgpu-hal` /
  exact-wgpu pin. A near drop-in plugin.
- **zero_copy** (opt-in) — Rive renders directly into a wgpu-allocated `VkImage` via a
  Bevy render-graph node, with no per-frame CPU readback. Non-blocking sync (Rive records
  into wgpu's own command buffer) plus an exact GPU-completion watermark (a Vulkan
  timeline semaphore). ABI-locked to Bevy 0.18.1's exact wgpu/wgpu-hal/ash versions.
  Validated on a real RTX 4090.

The feature surface is broad. Rendering and playback features are drawn automatically by
`advance()` + `draw()` — artboards (with selectable Fit + Alignment), linear animations,
state machines, shapes/paths, fills/strokes, gradients/dashes/trim-path/feather, blend
modes/clipping/draw-order, meshes + bones/skinning, constraints, the Yoga layout engine +
N-slice, solo, text, nested artboards/lists, Rive scripting, and embedded image/font
assets. Runtime control and data are wired explicitly: pointer input to
Listeners/joysticks (both tiers), view-model data binding (get/set across types, nested
paths, introspection, write-forwarding in both tiers), change/trigger observation, and
named/indexed artboard + state-machine selection. The full status table lives in
[`docs/feature-support.md`](docs/feature-support.md).

## Backends

| Backend | Status | Milestone | Effort |
|---------|--------|-----------|--------|
| Vulkan  | Shipping (floor + zero_copy) | — | — |
| D3D12   | Greenlit (clean port of the Vulkan zero_copy design) | M3a | Large |
| D3D11   | Out of scope (documented decision) | — | — |
| Metal   | Planned (needs a feasibility spike first) | M3b | XL |

### Vulkan — shipping

The primary dev/CI target. Both the floor and the zero_copy fast path run on Vulkan; the
fast path is validated on a real RTX 4090. On Windows, force `WGPU_BACKEND=vulkan` to use
the zero_copy tier today. Note that even the floor tier needs a Vulkan ICD present,
because Rive's offscreen device is self-managed Vulkan regardless of the wgpu backend.

### D3D12 — M3a, greenlit

A source-read feasibility spike concluded that D3D12 is a clean **port** of the Vulkan
zero_copy design, not a rewrite. Rive's D3D12 backend supports external command recording,
the GPU-completion watermark approach carries over, and the required C ABI stubs already
exist. The spike found exactly **one** adaptation: wgpu-hal's dx12 `CommandEncoder` has no
public accessor for its open command list (the Vulkan one does). The fallback is
straightforward — Rive records into its own command list, followed by one extra
same-queue submit. This is the default wgpu backend on Windows, so it unblocks the fast
path there without forcing Vulkan. Effort: Large.

### D3D11 — out of scope

This is a deliberate decision, not a deferral, for three concrete reasons:

1. Rive's D3D11 backend records onto a shared immediate device context and calls
   `ClearState()` in flush, which is incompatible with riding wgpu's command stream.
2. wgpu-hal has no dx11 backend at all.
3. wgpu's Windows default is D3D12, so there is no scenario where D3D11 beats D3D12.

### Metal — M3b, planned

Rive ships a Metal backend and the C ABI stubs exist, but Metal has not been spiked. It
needs a feasibility spike first, then the largest build delta of any backend (ObjC++/`.mm`
compilation) and net-new macOS CI. Covers macOS and iOS. Effort: XL.

## Runtime features

The authoritative, per-feature status table is the matrix in
[`docs/feature-support.md`](docs/feature-support.md). At a high level, rendering and
playback features are already drawn automatically by the renderer; the active backlog is
in runtime control and data — the channel between the host game and the Rive content.

Near-term planned items:

- Out-of-band asset loading
- Runtime text get/set
- Audio bridge
- Input devices (gamepad/keyboard/focus)
- Playback seek/pause controls
- Runtime bone/constraint/solo control

Deliberately excluded (deprecated by Rive, superseded by data binding):

- State-machine inputs
- Events read-back (replaced by change/trigger observation)

## CI / tooling

The Linux build + lint + doc + test job ships now and is GPU-free. Render, correctness,
and perf checks require a self-hosted GPU runner and are not yet in CI. The Windows-D3D12
and macOS-Metal CI matrices land alongside those backends (M3a and M3b respectively).

Work items are tracked as GitHub issues, labeled by milestone (`M3a`, `M3b`) and by area.
