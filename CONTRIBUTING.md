# Contributing to rive-rust

Thanks for your interest in contributing! rive-rust provides Rust bindings and a Bevy
plugin for Rive's native C++/Vulkan PLS renderer. Contributions of all kinds are
welcome — bug reports, fixes, features, docs, and examples.

The project is early / alpha, so APIs may still change. For anything non-trivial, it's
worth opening an issue to discuss the approach first.

## Building from source

The build guide is **[BUILD.md](BUILD.md)**. It covers the native toolchain (clang,
make, python3, glslang-tools, spirv-tools, libvulkan-dev, git, and premake5), the two
rendering tiers, and the `RIVE_PREBUILT_LIBS` shortcut that links pre-archived rive
static libs so you can skip the C++ build.

A from-scratch build compiles rive's native libraries (premake → make → clang) plus the
C++ shim, so the first build is slower than a typical Rust project. `RIVE_PREBUILT_LIBS`
is the fast path once you have those libs.

## Project conventions

A few conventions keep the codebase consistent:

- **The vendored runtime is read-only.** `vendor/rive-runtime` is a pinned git submodule
  of the upstream Rive runtime (tag `runtime-v0.1.106`). Please interact with it only
  through its public C++ API (from the shim layer) rather than patching it — if you need
  behaviour its API doesn't expose, open an issue and we'll find a path together.

- **One module per feature, across four layers.** Runtime-control features are wired
  across the C++ shim → FFI → safe wrapper → Bevy component/system, with one cohesive
  module per feature. The full convention (with worked examples) lives in
  **[docs/feature-support.md](docs/feature-support.md)** under *"How a feature is wired"*:

  1. **C++ shim** — `crates/rive-renderer-sys/shim/rive_shim_<feature>.cpp`
  2. **FFI** — `extern "C"` declarations in `crates/rive-renderer-sys/src/lib.rs`
  3. **Safe wrapper** — `crates/rive-renderer/src/<feature>.rs`
  4. **Bevy** — a `Component` + system in `crates/bevy-rive/src/<feature>.rs`

  Listing a feature as *supported* in `docs/feature-support.md` once it works end-to-end
  keeps the matrix honest.

## Before you push

CI runs a two-tier clippy check, docs, and the (non-GPU) tests. Running these locally
first keeps the feedback loop fast:

```sh
cargo clippy -p bevy-rive --features floor --all-targets -- -D warnings
cargo clippy -p bevy-rive --no-default-features --features zero_copy --all-targets -- -D warnings
cargo test --workspace
```

The `floor` and `zero_copy` tiers compile different code paths, so it's worth checking
both — a change that's clean in one can warn in the other. (`rustfmt` is not enforced
yet; match the surrounding style.)

## Running the examples

The examples don't bundle `.riv` assets, so point them at your own file (the first CLI
argument, or the `RIVE_RIV` env var) — see [assets/README.md](assets/README.md):

```sh
# Headless offscreen render to a PNG (floor tier; takes the path as a CLI arg)
cargo run -p rive-renderer --example offscreen_png -- path/to/file.riv out.png

# Floor-tier Bevy example (Bevy examples read RIVE_RIV)
RIVE_RIV=path/to/file.riv cargo run -p bevy-rive --features floor --example nimai_face

# zero_copy fast path (requires Vulkan)
WGPU_BACKEND=vulkan RIVE_RIV=path/to/file.riv cargo run -p bevy-rive \
  --no-default-features --features zero_copy --example sprite_riv_zerocopy
```

A couple of things worth knowing:

- The `zero_copy` tier is pinned to Bevy 0.18.1's exact graphics stack
  (wgpu 27.0.1 / wgpu-hal 27.0.4 / ash 0.38), because it shares a Vulkan device and
  command buffers with wgpu. Please don't `cargo update` those pins as part of an
  unrelated change; a dependency bump is welcome as its own focused PR.
- The `zero_copy` path is best validated on a Vulkan-capable GPU. WSL2 / Mesa Dozen is a
  handy dev environment but is non-conformant, so it's not a substitute for real-GPU
  validation — if your change touches that tier, a quick note on where you tested it
  (GPU, driver) in the PR is helpful.

## Pull requests

- Keep PRs focused on one logical change.
- Write a clear commit message (imperative summary; a *why* in the body when useful).
- Sign off your commits with the [Developer Certificate of Origin](https://developercertificate.org/):
  add a `Signed-off-by` line via `git commit -s`.
- Make sure fmt, both clippy tiers, and `cargo test --workspace` pass.
- By contributing, you agree your work is licensed under the project's **MIT** terms.

Please keep interactions respectful and constructive.

## Finding something to work on

Bugs, ideas, and questions are welcome in the
[issue tracker](https://github.com/Bantarus/rive-rust/issues). Issues labelled
[`good first issue`](https://github.com/Bantarus/rive-rust/issues?q=is%3Aissue+is%3Aopen+label%3A%22good+first+issue%22)
are a good place to start, and [docs/feature-support.md](docs/feature-support.md) shows
what's planned.
