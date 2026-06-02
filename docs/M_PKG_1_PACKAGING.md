# Milestone PKG.1 — make `bevy-rive` a robust, consumable dependency

The ECS API has been frozen and clean since M1a; the blocker to shipping `bevy-rive`
into a real Bevy game was **packaging**: the zero-copy tier is version-locked (`as_hal`
→ exact Bevy 0.18.1 / wgpu-hal 27.0.4 / ash 0.38) and the C++ toolchain travels with
the crate. This milestone splits those costs so the **floor tier drops in like a normal
plugin** and the **zero-copy tier is an explicit, fail-fast opt-in**, then **proves it
from an external consumer project** outside this workspace.

**Bottom line — shipped and proven.** The default (`floor`) build pulls **no `ash`, no
`wgpu-hal`, no exact-wgpu pin, no `as_hal` code** (only a caret `bevy = "0.18"` + a caret
`wgpu-types`); the locked `zero_copy` tier is an opt-in feature whose mismatch is a
**resolver error naming the versions**, not a baffling `as_hal` type error. A standalone
Bevy 0.18 app, OUTSIDE this workspace, adds `bevy-rive` as a path dependency with default
features and **renders the octopus** (42 792 colourful px, exit 0). The C++ toolchain is
removed from the consumer's hot path by a `RIVE_PREBUILT_LIBS` link mode (demonstrated:
the same consumer rebuilds with **0 premake/clang invocations**). The frozen M1a ECS API,
the Vulkan zero-copy behaviour, and the floor's rendering are untouched.

Target game project: **`/home/bantarus/DEV/voxelith`**.

---

## Step 0 — version alignment with the target game — **both tiers reachable as-is**

Voxelith is a Bevy workspace locked to Bevy 0.18.1 (it tracks an engine at
`../game-engine`). Its resolved graph is an **exact** match for `bevy-rive`'s pins:

| Dep | `bevy-rive` requires | voxelith `Cargo.lock` | reachable? |
|---|---|---|:--:|
| bevy | `0.18` (caret) | `0.18.1` | ✅ |
| wgpu | `=27.0.1` (zero-copy) | `27.0.1` | ✅ |
| wgpu-hal | `=27.0.4` (zero-copy) | `27.0.4` | ✅ |
| wgpu-types | `27` caret (floor) | `27.0.1` | ✅ |
| ash | `0.38` (zero-copy) | `0.38.0+1.3.281` | ✅ |

**No version gap.** The **floor** tier needs only the same Bevy major (0.18.x) — trivially
satisfied. The **zero-copy** tier needs the exact 0.18.1 / 27.0.1 / 27.0.4 / 0.38 triple —
voxelith already resolves it, so the fast path is reachable too (subject to the Vulkan
backend; voxelith defaults to whatever wgpu picks — `WGPU_BACKEND=vulkan` until the D3D12
port, see `docs/M3_0_D3D12_SPIKE.md`).

---

## Step 1 — feature split — **floor decoupled, zero-copy isolated**

`crates/bevy-rive/Cargo.toml`:
- **`floor` (default)** — `default = ["floor"]`, `floor = ["dep:wgpu-types"]`. The exact
  `wgpu-types = "=27.0.1"` pin became a **caret** `wgpu-types = { version = "27", optional
  = true }` (named only because `bevy_image` does not re-export `Image::new`'s
  `Extent3d`/`TextureFormat`/`TextureDimension` — they are a private `use` in its
  `image.rs:22`; the caret unifies with whatever `wgpu-types` the host Bevy links). Base
  `bevy` loosened `0.18.1` → `0.18`.
- **`zero_copy` (opt-in)** — unchanged: the exact `wgpu = =27.0.1` / `wgpu-hal = =27.0.4`
  / `ash = 0.38` pins + `bevy/{bevy_render,bevy_core_pipeline,raw_vulkan_init}`.

`crates/bevy-rive/src/lib.rs`: the **frozen public types** (the `RiveFile` asset + loader,
`RiveAnimation`/`RiveTarget`, the selector enums, the `RivePlugin` struct +
`register_asset`) stay **always-on** (they use only `Handle<Image>`, no wgpu types). The
M1a **machinery** (the `impl Plugin for RivePlugin`, the four NonSend systems, the native
instance map, `make_rive_image`, and the `wgpu_types` import) is gated `#[cfg(feature =
"floor")]`. A `compile_error!` fires if neither tier is enabled.

**Verification — clean gating both directions:**

```
# floor (default): the wgpu family present is ONLY wgpu-types (no ash/wgpu-hal/wgpu)
$ cargo tree -p bevy-rive --no-default-features --features floor
    └── wgpu-types v27.0.1          # caret-resolved; NO ash / wgpu-hal / wgpu

# zero_copy: pulls the locked tier
$ cargo tree -p bevy-rive --no-default-features --features zero_copy
    ├── ash v0.38.0+1.3.281
    ├── wgpu v27.0.1
    └── wgpu-hal v27.0.4

$ cargo check -p bevy-rive                                   # floor   → Finished 1.3s
$ cargo check -p bevy-rive --no-default-features --features zero_copy  # → Finished 17s
$ cargo check -p bevy-rive --no-default-features             # neither → compile_error:
  error: bevy-rive needs a rendering tier: enable `floor` … and/or `zero_copy` …
```

`cargo clippy` reports **0 bevy-rive Rust warnings** on both feature sets.

---

## Step 2 — fail-fast version guard (zero-copy) — **mismatch names the versions**

Three layers, so a mismatch is loud rather than a silent `as_hal` corruption:

1. **Resolver-level version lock (primary; compile-time).** The exact `=27.0.1` /
   `=27.0.4` pins make a host on a different wgpu a **resolution error that names the
   versions**. Demonstrated with a throwaway consumer forcing `wgpu = "=27.0.0"` against
   the zero-copy tier:
   ```
   error: failed to select a version for `wgpu`.
       ... required by package `bevy-rive`
       versions that meet the requirements `=27.0.1` are: 27.0.1     ← expected
       ... previously selected package `wgpu v27.0.0`                ← found
       ... which satisfies dependency `wgpu = "=27.0.0"` of `…demo`
       failed to select a version for `wgpu` which could resolve this conflict
   ```
   (A *different-major* wgpu does NOT conflict — Cargo lets majors coexist — so this is the
   faithful "a Bevy bump moved wgpu within 27.x" case.)
2. **Build-script advertisement (compile-time, visible).** `crates/bevy-rive/build.rs`
   emits, only when `zero_copy` is active, `cargo:warning=bevy-rive zero_copy tier is
   ABI-locked to Bevy 0.18.1 / wgpu 27.0.1 / wgpu-hal 27.0.4 / ash 0.38 …`. Silent on a
   floor build.
3. **Runtime backend guard (defence-in-depth).** The right version on the *wrong backend*
   compiles fine but `as_hal::<Vulkan>()` returns `None` (D3D12/Metal/GL) and the tier
   would silently do nothing. `extract_shared_handles_once` now logs that **once** (a
   `Local<bool>` latch — was per-frame) with the remedy: *"wgpu is NOT on the Vulkan
   backend — the shared-VkImage fast path is INERT … set `WGPU_BACKEND=vulkan` … or use
   the default `floor` tier."*

---

## Step 3 — clean consumer API surface — **the three-step flow, GPU machinery private**

`bevy_rive::prelude` is the whole public surface a game touches:
- floor: `RivePlugin`, `RiveAnimation`, `RiveTarget`, `RiveFile`, `ArtboardSelector`,
  `StateMachineSelector`;
- zero-copy adds `RiveZeroCopyPlugin` + `install_interlock_device_callback`.

The render-graph node, `as_hal` extraction, and watermark stay **private** (the
`zero_copy` module is `mod`, not `pub mod`; only the two zero-copy entry points are
re-exported). The crate docs state each tier's setup — floor = the three-step
`add_plugins(RivePlugin)` → spawn → display flow; zero-copy = callback-before-DefaultPlugins
+ `RiveZeroCopyPlugin` + `disable::<PipelinedRenderingPlugin>()` + `WGPU_BACKEND=vulkan` —
with the loud "every Bevy bump is a re-validation" warning. Two `no_run` doctests (one per
tier) compile and pass.

---

## Step 4 — prebuilt-libs link path — **the C++ toolchain off the hot path**

`crates/rive-renderer-sys/build.rs` honours **`RIVE_PREBUILT_LIBS=<dir>`**: when set it
links pre-archived libraries and **skips premake/make/clang *and* the rive-runtime
submodule** entirely (the branch returns before `ensure_submodule_present`). `<dir>` must
hold the ten rive static libs + the `rive_shim` archive (a shared
`rive_archive_file_names()` helper keeps the from-source verify and the prebuilt check in
lock-step); a missing archive fails fast listing what is absent.

**Demonstration (both paths work):**
```
# from-source still builds
$ cargo check -p rive-renderer-sys                      → Finished 5.52s

# produce the archive (tools/archive_prebuilt.sh) → 11 files, then relink the consumer:
$ cargo clean -p rive-renderer-sys --manifest-path <consumer>
$ RIVE_PREBUILT_LIBS=/tmp/rive-prebuilt cargo build --manifest-path <consumer>
  warning: rive-renderer-sys: linking PREBUILT libs … (skipped premake/make/clang + the
           rive-runtime submodule)
  Finished in 2.72s        ← premake/make/clang invocations: 0
$ ./target/debug/rive-bevy-consumer
  consumer: octopus rendered — 42792 colorful px → consumer_octopus.png   (exit 0)
```
The prebuilt-linked binary renders **identically** to the from-source one. Production +
naming are documented in **BUILD.md §8**; `tools/archive_prebuilt.sh` automates it. (Per-
platform binary hosting + CI is the follow-on; this establishes the link mechanism. The
archive is ABI-specific — one set per toolchain × profile × target.)

---

## Step 5 — external-consumer integration test — **the proof**

A standalone project at **`/home/bantarus/DEV/rive-bevy-consumer`** (sibling to this repo,
its own `[workspace]` + `target/`), adding `bevy-rive` as a **path dependency with default
features** (the `floor` tier). Its complete `Cargo.toml` dependencies:

```toml
[dependencies]
bevy-rive = { path = "../rive-rust/crates/bevy-rive" }        # default features = floor
bevy = { version = "0.18", default-features = false, features = ["bevy_asset", "bevy_image", "bevy_log"] }
image = "0.25"
```

`src/main.rs` uses the exact public flow — `use bevy_rive::prelude::*;`,
`add_plugins(RivePlugin)`, `spawn((RiveAnimation::new(handle), RiveTarget::new(512, 512)))`
— **headless** (`MinimalPlugins` + `AssetPlugin` + `init_asset::<Image>()`; the floor needs
no render world, since display is the consumer's job), loads `octopus_loop.riv` through the
standard `AssetServer`, then reads the filled `Image` back and dumps it to PNG.

**Result — it renders.**
```
$ cargo build              # 215 crates; NO bevy_render / winit / wgpu / sprite pulled
$ ./target/debug/rive-bevy-consumer
  consumer: loading `octopus_loop.riv` via the standard AssetServer flow
  WARNING: dzn is not a conformant Vulkan implementation, testing use only.   (Dozen → 4090)
  consumer: octopus rendered — 42792 colorful px of 262144 → consumer_octopus.png
  consumer: SUCCESS — bevy-rive floor works as an external dependency
  rc=0
```
`consumer_octopus.png` is the reference octopus — upright, pink body, blue hat, tentacles,
on the 0x303030 clear. The headless floor pulled **215 crates and none of
`bevy_render`/`winit`/`wgpu`/`sprite`** — concrete evidence of the decoupling.

**What the consumer's build required.** The from-source build runs
`rive-renderer-sys`'s `build.rs`, which invokes the full C++ pipeline (premake5 → make →
clang for the shim). It was fast here (≈25 s) only because the rive static libs already
existed in this repo's shared in-tree `out/` dir, so `make` rebuilt nothing; a consumer on
a clean machine pays the full multi-minute C++ build and needs clang + premake5 + make +
python3 + glslang + spirv-opt + the Vulkan loader on PATH. **Step 4's `RIVE_PREBUILT_LIBS`
removes that** — the same consumer rebuilt with 0 premake/clang invocations (above).

---

## Guardrails honoured
- **Correctness over convenience.** The zero-copy pins were NOT loosened to ease
  consumption — the fail-fast resolver lock + the runtime backend guard are the answer.
- **Frozen surfaces untouched.** The M1a ECS type surface, the Vulkan zero-copy behaviour,
  and the floor's rendering are unchanged; this is packaging only (the floor renders the
  byte-identical octopus, before and after).

## Follow-ons (out of scope here)
- **Prebuilt binary distribution** — per-platform CI + hosting, so consumers never build
  C++ (turns "build the libs once" into "download them").
- **Zero-copy self-wiring spike** — whether `RivePlugin` can inject `RawVulkanInitSettings`
  before `RenderPlugin` builds the device, collapsing zero-copy setup to "add the plugin +
  disable pipelining"; then a zero-copy external-consumer test (needs `WGPU_BACKEND=vulkan`
  + a window, so not headless).
- **D3D12 fast path (M3a)** — would let voxelith's *default* (D3D12) backend reach the
  zero-copy tier without forcing Vulkan (greenlit in `docs/M3_0_D3D12_SPIKE.md`).
