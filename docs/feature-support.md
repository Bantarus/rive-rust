# Rive feature support — status & roadmap

This is the living map of **which Rive runtime features `rive-rust` exposes**, and the
plan to reach **all of them** (except the obsolete ones called out at the bottom). It is
the roadmap for the engine-plugin work; pair it with the C++ runtime reference in
[`docs/cpp/`](cpp/) (each row links the relevant doc) and the architecture spec
[`docs/engine-plugin-rive-spec.md`](engine-plugin-rive-spec.md).

## Legend

| Mark | Meaning |
|------|---------|
| ✅ | Supported end-to-end (shim → FFI → safe wrapper → bevy-rive), verified |
| 🟡 | Partial — a first slice ships; gaps noted |
| 🔜 | Planned (not yet wired) — priority in the backlog below |
| ⛔ | Excluded — obsolete / not-applicable (see bottom) |

A key distinction runs through the table:

- **Rendering & playback** features are drawn **automatically** by `advance()` + `draw()`
  — they need *no* per-feature API and are ✅ the moment the artboard renders. The only
  gaps here are inputs the runtime can't synthesize itself (out-of-band assets, audio).
- **Runtime control & data** features need an explicit FFI/API to drive or read them.
  This is where the real backlog is — the game↔face channel.

---

## How a feature is wired (the maintainability contract)

Every runtime-control feature crosses **four layers**, one cohesive module/TU per feature
area so the codebase stays navigable as it grows toward full coverage:

| Layer | Crate / file | What it holds |
|-------|--------------|---------------|
| 1. C++ shim | `crates/rive-renderer-sys/shim/rive_shim_<feature>.cpp` (+ shared structs in `rive_shim_internal.hpp`, ABI in `rive_shim.h`) | `extern "C"` calls into vendored rive (which stays **pristine** — public API only) |
| 2. FFI | `crates/rive-renderer-sys/src/lib.rs` (banner section per feature) | raw `extern "C"` declarations |
| 3. Safe wrapper | `crates/rive-renderer/src/<feature>.rs` | `Result`-based, typed methods (e.g. `impl Artboard`) |
| 4. Bevy | `crates/bevy-rive/src/<feature>.rs` | a `Component` + the system wiring in the floor advance loop |

**Convention for a new feature:** add one `rive_shim_<feature>.cpp` (register it in
`build.rs`), declare the FFI under a `// ===== <feature> =====` banner, add a
`crates/rive-renderer/src/<feature>.rs` module (`mod` + `pub use` from `lib.rs`), and a
`crates/bevy-rive/src/<feature>.rs` component + system. Worked examples in the tree:
**pointer input** (`rive_shim.cpp` pointer fns → `StateMachine::pointer_*` → `RivePointer`)
and **view-model data binding** (`rive_shim_viewmodel.cpp` → `Artboard::vm_*` in
`view_model.rs` → `RiveViewModel`).

> Migration note: the older render/frame/context/state-machine code still lives in the
> monolithic `rive_shim.cpp` and `rive-renderer/src/lib.rs`; it is being split into
> per-feature modules incrementally (new features already follow the convention above).

---

## Rendering & playback (automatic via `advance` + `draw`)

| Feature | Status | `docs/cpp` | Notes |
|---------|:------:|------------|-------|
| Artboard render (Fit::contain/center) | ✅ | [rendering-loop](cpp/rendering-loop.mdx), [renderers](cpp/renderers.mdx) | offscreen (floor) + zero-copy Vulkan tiers; atlas batching/tiling for many faces |
| Linear animations (as the default scene) | ✅ | [state-machines](cpp/state-machines.mdx) | played when an artboard has no default state machine |
| State machines (advance/apply) | ✅ | [state-machines](cpp/state-machines.mdx) | `advanceAndApply`; the playback unit |
| Shapes / paths / vertices | ✅ | [renderers](cpp/renderers.mdx) | rectangles, ellipses, polygons, stars, paths |
| Fills, strokes, caps/joins | ✅ | — | drawn by the PLS renderer |
| Gradients (linear/radial), dashes, trim path, feather | ✅ | — | paint effects render as authored |
| Blend modes, clipping, draw order / draw targets | ✅ | — | full PLS path |
| Meshes / vertex deform, bones / skinning | ✅ | — | rendered; no runtime bone API yet (control 🔜) |
| Constraints (IK, distance, follow-path, transform, …) | ✅ | — | solved during advance; runtime control 🔜 |
| Layout engine (Yoga flex), N-slice (9-patch), follow-path | ✅ | — | solved during advance; resize via target size |
| Solo (exclusive visibility) | ✅ | — | rendered; runtime toggle API 🔜 |
| Text rendering (runs, modifiers, styles, text-follow-path) | ✅ | — | renders embedded text; **runtime text get/set** 🔜 |
| Nested artboards / artboard lists | ✅ | [file-and-artboard](cpp/file-and-artboard.mdx) | rendered; per-child runtime access 🔜 |
| Scripting — autonomous nodes (e.g. BallBreath) | ✅ | — | needs `--with_rive_scripting` + a **Publish-signed** `.riv` + the shim VM bind (shipped) |
| Embedded image / font assets | ✅ | [asset-loading](cpp/asset-loading.mdx) | in-band assets decode automatically |

## Runtime control & data (need an FFI/API)

| Feature | Status | `docs/cpp` | Notes |
|---------|:------:|------------|-------|
| Advance / playback tick | ✅ | [state-machines](cpp/state-machines.mdx) | `StateMachine::advance`; `RiveAnimation.speed` |
| Pointer input → Listeners / joysticks | ✅ | [state-machines](cpp/state-machines.mdx) | move/down/up/exit; `RivePointer` (floor). zero-copy/atlas-tile mapping 🔜 |
| **View-model data binding** | 🟡 | [data-binding](cpp/data-binding.mdx) | **number/bool/trigger/color/string/enum get/set + top-level introspection** ✅ (floor); `RiveViewModel` (queued writes + typed watch read-back). **Gaps:** nested-VM introspection, lists, image/artboard ref props, zero-copy forwarding |
| State-machine inputs (bool/number/trigger) | 🔜 | [state-machines](cpp/state-machines.mdx) | the classic control path; `Scene::getBool/getNumber/getTrigger`. (Data binding is the modern path) |
| Events read-back (state changes, custom / open-url / audio) | 🔜 | [state-machines](cpp/state-machines.mdx) | `stateChanged*` + `reportedEvent*` → an ECS event each frame so gameplay reacts |
| Named artboard / state-machine selection | 🔜 | [file-and-artboard](cpp/file-and-artboard.mdx) | `ArtboardSelector` / `StateMachineSelector` reserved; only `Default` honored today |
| Runtime text value get/set | 🔜 | — | `TextValueRun` — set/read a text run's string |
| Out-of-band asset loading (images/fonts/audio) | 🔜 | [asset-loading](cpp/asset-loading.mdx) | `FileAssetLoader` callback → supply textures/fonts the `.riv` references externally |
| Audio playback | 🔜 | — | `WITH_RIVE_AUDIO` + an engine audio bridge (route to the host mixer) |
| Joystick / gamepad / keyboard / focus input | 🔜 | — | `Scene` gamepad/keyboard + `FocusManager`; for game-controlled rigs |
| Animation playback controls (seek / pause / per-anim speed) | 🔜 | — | direct `LinearAnimationInstance` time control beyond the SM |
| Bones / constraints / solo runtime control | 🔜 | — | drive bones, toggle solo children, set constraint strength at runtime |

---

## Priority backlog (next features, ROI-ordered)

1. **View-model data binding — remaining:** nested-VM introspection, lists, image/artboard
   reference props, then **zero-copy-tier forwarding** of `RiveViewModel`. (Slice-2
   color/string/enum shipped; setting `viseme` is headless-verified to change the mouth.)
2. **State-machine inputs** (bool/number/trigger) — the other half of the write channel,
   for `.riv` content authored without view models.
3. **Events read-back** — the read channel: surface state changes + custom/open-url/audio
   events as Bevy events so gameplay reacts to the face.
4. **Named artboard / state-machine selection** — honor `ArtboardSelector::ByName/ByIndex`.
5. **Out-of-band asset loading** — `FileAssetLoader` for externally-supplied images/fonts.
6. **Runtime text get/set**; **atlas-tile pointer mapping** (zero-copy); **audio bridge**.

---

## Excluded (obsolete / not-applicable)

| Item | Why |
|------|-----|
| Low-level `LinearAnimationInstance` direct playback | Superseded by state machines as the playback unit; we fall back to the default scene only when no SM exists. (Seek/pause control may still be added under "playback controls".) |
| `CommandQueue` / `CommandServer` | An *alternative* thread-decoupled API; we drive rive directly from the render-adjacent main thread (NonSend), so it is redundant, not additive. |
| `WITH_RIVE_TOOLS` editor surface | Editor/tooling mode that alters core runtime behavior (blanks rendering); never enabled in a playback runtime. |
| Deprecated `Factory` paths (e.g. `makeEmptyRenderPath`) | Legacy; the current `RenderContext` factory path is used. |
| Test/`#ifdef TESTING` hooks | Not part of the shipping runtime. |
