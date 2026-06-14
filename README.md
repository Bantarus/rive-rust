# rive-rust

Rust bindings and a Bevy plugin for Rive's **native C++/Vulkan PLS renderer** — the real
[rive-runtime](https://github.com/rive-app/rive-runtime), not a reimplementation. `rive-rust` builds
Rive's own renderer as a static library, drives an artboard and state machine through `advance()` +
`draw()`, and lands the result in a [Bevy](https://bevyengine.org) `Handle<Image>` you can show on a
2D sprite or map onto a 3D material. You get Rive's full rendering and playback feature set — vector
shapes, gradients, meshes and skinning, the Yoga layout engine, text, nested artboards, scripting —
plus a growing runtime-control surface (pointer input, view-model data binding) on top.

> **Status: alpha.** All crates are version `0.0.0`, `publish = false`, and **not on crates.io**. The
> public API may change without notice. See [Status & maturity](#status--maturity).

## How it works

Every runtime-control feature crosses a **four-layer stack**, kept as one cohesive module per feature:

```
C++ shim  ──►  FFI declarations  ──►  safe Rust wrapper  ──►  Bevy component + system
(sys/shim)     (rive-renderer-sys)    (rive-renderer)         (bevy-rive)
```

Rendering and playback features (shapes, gradients, meshes, layout, text, scripting, …) are drawn
**automatically** by `advance()` + `draw()` and need no per-feature API.

### Two rendering tiers

The Bevy plugin ships two ways to get Rive's pixels into a Bevy `Image`. They are selected by Cargo
feature.

| Tier | Feature | How pixels arrive | Device & sync | Coupling |
|------|---------|-------------------|---------------|----------|
| **floor** (default) | `floor` | Rive renders **offscreen on its own Vulkan device**; the pixels are read back and copied into a Bevy `Image` each frame. | Self-managed Vulkan device, independent of wgpu. A Vulkan ICD must be present even when wgpu is on another backend. | Loosely coupled. No `ash` / `wgpu-hal` / exact-wgpu pin — a near-drop-in plugin (caret `bevy 0.18` + `wgpu-types`). |
| **zero_copy** (opt-in) | `zero_copy` | Rive renders **directly into a wgpu-allocated `VkImage`** via a Bevy render-graph node — no per-frame CPU readback. | Device sharing via Bevy's `raw_vulkan_init`; non-blocking sync (Rive records into wgpu's own command buffer) plus an exact GPU-completion watermark (a Vulkan timeline semaphore). | ABI-locked to Bevy 0.18.1's **exact** wgpu `27.0.1` / wgpu-hal `27.0.4` / ash `0.38`. A version skew is a resolver error by design. Validated on a real RTX 4090. |

The `floor` is the path a normal game project drops in like any other Bevy plugin. The `zero_copy`
fast path trades that loose coupling for eliminating the per-frame readback.

## Platform & backend status

| Platform | Backend | Floor (CPU-copy) | Zero-copy fast path |
|----------|---------|------------------|---------------------|
| **Linux** | Vulkan | ✅ Shipping (primary dev/CI target) | ✅ Shipping — validated on a real RTX 4090 |
| **Windows** | D3D12 | ✅ Runs today (still needs a Vulkan ICD for Rive's offscreen device) | 🔜 Vulkan today (`WGPU_BACKEND=vulkan`); a native D3D12 fast path is greenlit as **M3a** |
| **macOS / iOS** | Metal | 🔜 Planned (**M3b**) — needs a feasibility spike | 🔜 Planned (**M3b**) |
| **Android** | Vulkan | 🔜 Target, not yet exercised | 🔜 Target, not yet exercised |

D3D11 is **out of scope** (a documented decision, not a deferral). See the
[roadmap](#documentation) for the backend rationale.

> **WSL2 (Mesa Dozen) is a development environment only** — atomic-only and non-conformant. It is
> never a deploy or CI target.

## Features

Rive's **rendering & playback** features are all supported and drawn automatically once the artboard
renders:

- Artboards with selectable **Fit** + **Alignment**, linear animations, state machines
- Shapes/paths, fills/strokes, gradients, dashes, trim-path, feather
- Blend modes, clipping, draw-order, meshes + bones/skinning
- Constraints (IK / distance / follow-path / transform)
- The layout engine (Yoga flex) + N-slice, solo
- Text rendering, nested artboards / lists
- Rive **scripting** (autonomous nodes), embedded image/font assets

**Runtime control & data** (the four-layer features):

- **Pointer input** → Listeners / joysticks (both tiers, dedicated path)
- **View-model data binding** — get/set number, bool, trigger, color, string, enum; nested paths;
  introspection; write-forwarding in both tiers _(partial — read-back is floor-only and some bits are
  still in progress; see the [feature matrix](docs/feature-support.md))_
- **Change / trigger observation** — the modern replacement for the deprecated events read-back
- **Named / indexed artboard + state-machine selection**

Planned: out-of-band asset loading, runtime text get/set, audio bridge, gamepad/keyboard/focus input,
playback seek/pause controls, runtime bone/constraint/solo control. State-machine inputs and events
read-back are **out of scope** (deprecated by Rive, superseded by data binding).

See **[docs/feature-support.md](docs/feature-support.md)** for the full living feature matrix and
roadmap detail.

## Quick start

### Prerequisites

Building the native renderer needs `clang` (not gcc), `make`, `python3`, `glslang-tools`,
`spirv-tools`, `libvulkan-dev`, `git`, and `premake5`. The canonical, step-by-step guide —
including the `RIVE_PREBUILT_LIBS` shortcut that links pre-archived libs and skips the C++ toolchain —
is in **[BUILD.md](BUILD.md)**.

### Supply a `.riv`

The examples are **not** bundled with any `.riv` file; a fresh clone has none. The headless
`offscreen_png` example takes the `.riv` path as its **first CLI argument**; the Bevy examples read
it from the **`RIVE_RIV`** environment variable. Grab one from
[rive.app/community](https://rive.app/community) or the
[awesome-rive](https://github.com/rive-app/awesome-rive) repo. See
**[assets/README.md](assets/README.md)** for details.

### Run the floor (default tier)

```sh
RIVE_RIV=path/to/file.riv cargo run -p bevy-rive --features floor --example sprite_riv
```

Headless offscreen render that writes a PNG (no window):

```sh
cargo run -p rive-renderer --example offscreen_png -- path/to/file.riv out.png
```

### Run the zero_copy fast path

Requires the `zero_copy` feature and a native Vulkan backend:

```sh
WGPU_BACKEND=vulkan RIVE_RIV=path/to/file.riv cargo run -p bevy-rive \
  --no-default-features --features zero_copy --example sprite_riv_zerocopy
```

## Crates

The workspace lives under `crates/`.

| Crate | Role |
|-------|------|
| [`rive-renderer-sys`](crates/rive-renderer-sys) | Raw FFI + a `build.rs` that builds Rive's native static libs (premake → make → clang) and compiles the C++ shim. `RIVE_PREBUILT_LIBS=<dir>` links pre-archived libs and skips the toolchain. |
| [`rive-renderer`](crates/rive-renderer) | The safe Rust wrapper: `Context`, `File`, `Artboard`, `StateMachine`, `RenderTarget`, value types, unpremultiply. |
| [`bevy-rive`](crates/bevy-rive) | The Bevy plugin and prelude. Feature split: `floor` (default) / `zero_copy` (opt-in). |

## Documentation

- **[BUILD.md](BUILD.md)** — the canonical build guide (toolchain, `RIVE_PREBUILT_LIBS`, platforms).
- **[docs/feature-support.md](docs/feature-support.md)** — the full feature matrix and per-feature
  roadmap.
- **[ROADMAP.md](ROADMAP.md)** — milestones and the backend roadmap (M3 / D3D12 / Metal).
- **[docs/architecture.md](docs/architecture.md)** — the four-layer stack and rendering-tier internals.
- **[CONTRIBUTING.md](CONTRIBUTING.md)** — how to build, test, and propose changes.

## Status & maturity

Early / alpha. All three crates are at version `0.0.0` with `publish = false` and are **not published
to crates.io**. The public API is unstable and may change without notice between commits. MSRV is
Rust **1.94**; the Bevy integration targets **0.18.1**.

The vendored `rive-runtime` is a **pristine** git submodule pinned to tag `runtime-v0.1.106` — no
patches, ever.

## License

This project's own code is licensed under the **MIT License** — see
[LICENSE-MIT](LICENSE-MIT) (and [NOTICE](NOTICE)).

The vendored `rive-runtime` under `vendor/rive-runtime` is also **MIT** (Copyright 2020 Rive).

`.riv` assets are **not redistributed** with this repository and are intentionally gitignored; supply
your own per [assets/README.md](assets/README.md).
