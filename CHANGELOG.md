# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). This project is pre-1.0
and under active development; all crates are currently `0.0.0` (not yet published to
crates.io), so changes are tracked under **Unreleased** until the first release.

## [Unreleased]

The current capability baseline:

### Rendering
- Native Rive C++/Vulkan PLS renderer integrated into Bevy.
- **`floor` tier** (default): CPU-copy offscreen render into a Bevy `Image`.
- **`zero_copy` tier** (opt-in): rive renders directly into a wgpu-allocated
  `VkImage` via a render-graph node, with non-blocking sync and an exact
  GPU-completion watermark; optional atlas batching for many faces.
- Selectable `Fit` + `Alignment` per face (both tiers).

### Runtime control & data
- Pointer input → state-machine Listeners / joysticks (both tiers, dedicated path).
- View-model **data binding**: get/set number, bool, trigger, color, string, enum
  (flat + nested paths), nested-VM + list introspection, write-forwarding in both
  tiers. _(Partial — see [`docs/feature-support.md`](docs/feature-support.md).)_
- View-model **change/trigger observation** (the modern replacement for the
  deprecated events read-back).
- Named / indexed artboard + state-machine selection.

### Tooling
- Consumable packaging: `floor` / `zero_copy` feature split, fail-fast version
  guards, and a `RIVE_PREBUILT_LIBS` link path to skip the C++ toolchain.
- Linux CI (`fmt` + build + clippy both tiers + doc + test, GPU-free).

[Unreleased]: https://github.com/Bantarus/rive-rust/commits/master
