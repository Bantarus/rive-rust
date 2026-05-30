# Building `rive-rust` (Milestone 0)

Milestone 0 renders a `.riv` file's default state machine to an offscreen image
using the **native Rive Renderer** (rive-runtime's PLS renderer, Vulkan backend),
reads the pixels back, and writes a PNG. The shim manages its **own**
`VkInstance`/`VkDevice` — there is no wgpu or Bevy yet.

```
cargo run --example offscreen_png -- assets/coffee_loader.riv out.png
```

---

## 1. Prerequisites

| Tool | Used in this milestone | Why |
| --- | --- | --- |
| Rust (stable) | `rustc 1.94.1` | build the crates |
| clang / clang++ | `18.1.3` | compile rive-runtime + the C++ shim (gcc is **not** supported — rive relies on clang vector builtins) |
| premake5 | `5.0.0-beta2` (vendored) | generate rive-runtime's makefiles |
| GNU make | `4.3` | run the generated makefiles + the shader build |
| python3 | `3.12.3` | rive's offline shader minify/codegen |
| glslangValidator | `15.1.0` (`glslang-tools`) | compile rive's Vulkan GLSL → SPIR-V |
| spirv-opt | `2025.1` (`spirv-tools`) | optimize rive's SPIR-V |
| git | `2.43` | premake clones Vulkan-Headers / VMA / harfbuzz / … at configure time |
| Vulkan loader | `libvulkan.so.1` + `libvulkan-dev` | link `-lvulkan`; a working ICD at runtime |

### Install (Ubuntu 24.04 / WSL2)

```bash
sudo apt-get update
sudo apt-get install -y clang make python3 glslang-tools spirv-tools libvulkan-dev pkg-config vulkan-tools
```

`premake5` is **not** in apt, and the binary is **not** committed (binaries
don't belong in git). Fetch the pinned `5.0.0-beta2` Linux binary (matching
rive-runtime's own Dockerfile) into `tools/premake5`, which is git-ignored:

```bash
tools/fetch_premake.sh
```

`build.rs` looks for premake5 in this order: `$PREMAKE5` → `tools/premake5` → `PATH`.

The Vulkan **headers** themselves are *not* required from the system: premake
clones `KhronosGroup/Vulkan-Headers` (pinned `vulkan-sdk-1.4.321`) and
`VulkanMemoryAllocator` (`v3.3.0`) into `.rive-deps/` and the shim compiles
against those. `libvulkan-dev` is only needed for the loader's dev symlink.

---

## 2. Submodule

rive-runtime is a git submodule under `vendor/`, pinned to commit
`3f868558a4596e153afdb6bc3e8058596f0d971d` (`.version` 0.1). After cloning this
repo:

```bash
git submodule update --init --recursive
```

---

## 3. What `build.rs` does (crates/rive-renderer-sys)

1. Detects the tools above and fails with an actionable message if any are missing.
2. Runs `premake5 gmake2` **from `vendor/rive-runtime/renderer/`** with
   `--config=debug --out=out/rive-rust-m0 --with_vulkan --with_rive_text --with_rive_layout`.
   - Working directory matters: premake anchors `RIVE_BUILD_OUT` and the
     generated-shader include path to its CWD, so it must run from `renderer/`.
   - `--out` must be **relative** (premake concatenates it onto the CWD).
   - This step also runs rive's offline shader build (needs python3 + make +
     glslangValidator + spirv-opt), and clones the pinned deps into `.rive-deps/`
     (`$DEPENDENCIES`).
3. Runs `make -C <out> config=default -j<N>` with **explicit** library targets
   (never `all`, which would build the `path_fiddle` demo and require GLFW):
   `rive rive_pls_renderer rive_decoders libpng zlib libjpeg libwebp rive_harfbuzz rive_sheenbidi rive_yoga`.
4. Compiles `shim/rive_shim.cpp` **plus rive's `rive_vk_bootstrap` sources** with
   clang (`-std=c++17 -fno-rtti -DRIVE_VULKAN -DVK_NO_PROTOTYPES`). `rive_vk_bootstrap`
   is *not* built into any rive static lib, so it is compiled into the shim here.
5. Emits the link directives (static rive libs in single-pass order, then
   `-lvulkan -lstdc++ -lpthread -ldl -lm`).

Artifacts land in `vendor/rive-runtime/renderer/out/rive-rust-m0/` and
`.rive-deps/` (both git-ignored). A clean rebuild: `cargo clean` and delete
those two directories.

> The rive libs are built in **debug** by default. Debug avoids release LTO,
> whose LLVM-bitcode archives can confuse a non-LLVM final linker. Override with
> `RIVE_RUNTIME_CONFIG=release` once you've confirmed your linker handles LTO.

---

## 4. Runtime environment variables

- `RIVE_GPU=<substring>` — pick a GPU by name substring (`integrated` selects an
  integrated GPU). Useful on WSL2 to choose between `Dozen` and `llvmpipe`.
- `RIVE_FORCE_ATOMIC=1` — force the renderer's atomic PLS path (fallback when
  fragment-shader-interlock / rasterizer-ordered access is unavailable).

---

## 5. Gotchas hit while bringing M0 up

- **premake-on-Linux**: works with the official `5.0.0-beta2` binary. Must run
  from `renderer/`; `--out` relative; the only make *configuration* is literally
  `default` (debug/release is baked at premake time). See §3.
- **Double-`lib` archive names**: premake prefixes `lib`, so the `libpng` /
  `libjpeg` / `libwebp` projects produce `liblibpng.a` / `liblibjpeg.a` /
  `liblibwebp.a` (link names `libpng` / `libjpeg` / `libwebp`). `zlib` →
  `libzlib.a`, `rive` → `librive.a`.
- **Link order** (single-pass GNU ld, no `--start-group` needed because the
  graph is acyclic): shim → `rive_pls_renderer` → `rive` → `rive_decoders` →
  `libpng` → `zlib` → `libjpeg` → `libwebp` → `rive_harfbuzz` →
  `rive_sheenbidi` → `rive_yoga` → system libs.
- **`rive_vk_bootstrap` is not a static lib** — compile its sources into the shim.
- **WSL2 / NVIDIA**: there is **no native NVIDIA Vulkan ICD** under WSL2 — only
  `Dozen` (Mesa's Vulkan→D3D12 layer wrapping the RTX 4090) and `llvmpipe` (CPU).
  Neither is likely to expose `VK_EXT_fragment_shader_interlock` /
  `VK_EXT_rasterization_order_attachment_access`, so the renderer uses its
  **atomic** fallback path. This is correct, just slower, and is the main thing
  M1's wgpu shared-device plan must account for. If `Dozen` misbehaves, set
  `RIVE_GPU=llvmpipe` for a guaranteed-correct (software) reference image.
- **Color contract** (so M1 has a trustworthy reference to diff against): the
  offscreen target is `VK_FORMAT_R8G8B8A8_UNORM` (non-sRGB → no hardware gamma
  conversion; the bytes are sRGB-encoded, exactly what a PNG wants — **do not**
  apply gamma). The renderer outputs **premultiplied** alpha. **Orientation:**
  rive's Vulkan backend renders top-down, but `getPixelsFromLastImageCopy` flips
  rows to a GL-style bottom-up convention (rive's own PNG writer flips a *second*
  time to compensate). The shim flips back, so `read_pixels` returns genuine
  **top-down** RGBA8 — encode the PNG with no extra flip. (Skipping this flip
  renders the image upside down.) The example clears to an **opaque** color,
  making premultiplied == straight; for a transparent background, call
  `rive_renderer::unpremultiply_rgba8`.

---

## 6. Version triple to pin for M1 (do NOT add these yet)

M1 introduces wgpu shared-device interop. The Bevy ↔ wgpu ↔ ash versions must
match **exactly** (`as_hal` / `create_texture_from_hal` are unstable internal
wgpu APIs, and a raw `ash` version must match the one wgpu was built against).
When M1 starts, pin and record the triple here, e.g.:

```
bevy = "0.XX"      # ships wgpu X.Y
wgpu = "X.Y"       # must match Bevy's pinned wgpu exactly
ash  = "0.Z"       # must match wgpu's ash
```

Treat every Bevy bump as a deliberate interop re-validation, not a `cargo update`.

---

## 7. Test assets

`assets/` contains small, vector-only samples copied from rive-runtime's own
`renderer/webgpu_player/rivs/`:

- `coffee_loader.riv` (default) — a small vector loader animation.
- `octopus_loop.riv` — a looping vector animation.

More `.riv` files: rive's [awesome-rive](https://github.com/rive-app/awesome-rive)
repo, or anything exported from the Rive editor. M0 uses no image decoders for
its samples; an image-bearing `.riv` would need the (already-linked) decoders.
