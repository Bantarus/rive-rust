# Rive feature support — status & roadmap

This is the living map of **which Rive runtime features `rive-rust` exposes**, and the
plan to reach **all of them** (except the obsolete ones called out at the bottom). It is
the roadmap for the engine-plugin work; pair it with Rive's runtime reference at
[rive.app/docs](https://rive.app/docs) (each row links the relevant doc).

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

| Feature | Status | Reference | Notes |
|---------|:------:|-----------|-------|
| Artboard render (selectable Fit + Alignment) | ✅ | [layout](https://rive.app/docs/runtimes/layout) | offscreen (floor) + zero-copy Vulkan tiers; atlas batching/tiling for many faces. **`RiveFit { fit, alignment, scale_factor }`** component — all 8 rive fits (contain/cover/fill/none/layout/…) + 9 alignments, both tiers (dedicated + atlas); default contain/center. Pointer inversion tracks it. `None` = render at scale 1.0 (content grows in px, font constant) — e.g. an auto-resizing speech bubble |
| Linear animations (as the default scene) | ✅ | [state-machines](https://rive.app/docs/runtimes/state-machines) | played when an artboard has no default state machine |
| State machines (advance/apply) | ✅ | [state-machines](https://rive.app/docs/runtimes/state-machines) | `advanceAndApply`; the playback unit |
| Shapes / paths / vertices | ✅ | — | rectangles, ellipses, polygons, stars, paths |
| Fills, strokes, caps/joins | ✅ | — | drawn by the PLS renderer |
| Gradients (linear/radial), dashes, trim path, feather | ✅ | — | paint effects render as authored |
| Blend modes, clipping, draw order / draw targets | ✅ | — | full PLS path |
| Meshes / vertex deform, bones / skinning | ✅ | — | rendered; no runtime bone API yet (control 🔜) |
| Constraints (IK, distance, follow-path, transform, …) | ✅ | — | solved during advance; runtime control 🔜 |
| Layout engine (Yoga flex), N-slice (9-patch), follow-path | ✅ | — | solved during advance; resize via target size |
| Solo (exclusive visibility) | ✅ | — | rendered; runtime toggle API 🔜 |
| Text rendering (runs, modifiers, styles, text-follow-path) | ✅ | — | renders embedded text; **runtime text get/set** 🔜 |
| Nested artboards / artboard lists | ✅ | [artboards](https://rive.app/docs/runtimes/artboards) | rendered; per-child runtime access 🔜 |
| Scripting — autonomous nodes (e.g. BallBreath) | ✅ | — | needs `--with_rive_scripting` + a **Publish-signed** `.riv` + the shim VM bind (shipped) |
| Embedded image / font assets | ✅ | [loading-assets](https://rive.app/docs/runtimes/loading-assets) | in-band assets decode automatically |

## Runtime control & data (need an FFI/API)

| Feature | Status | Reference | Notes |
|---------|:------:|-----------|-------|
| Advance / playback tick | ✅ | [state-machines](https://rive.app/docs/runtimes/state-machines) | `StateMachine::advance`; `RiveAnimation.speed` |
| Pointer input → Listeners / joysticks | ✅ | [state-machines](https://rive.app/docs/runtimes/state-machines) | move/down/up/exit; `RivePointer` — **both tiers AND every zero-copy draw path** (floor + zero-copy **dedicated** + zero-copy **atlas** tiles). The inversion tracks the face's Fit/Alignment; atlas faces are tile-aware (target-pixel coords are normalized into the face's tile before inverting, via `set_pointer_tile`) |
| **View-model data binding** | 🟡 | [data-binding](https://rive.app/docs/runtimes/data-binding) | get/set **number/bool/trigger/color/string/enum** (flat + `/`-nested paths) ✅; **introspection incl. nested VMs + lists** via the borrowed `RiveViewModelInstance` handle (`Artboard::vm_root` → `view_model`/`list_size`/`list_item` + reads) ✅; **per-item / nested writes** ✅ — the handle is now read-**write** (`set_*` / `fire_trigger`), and `Artboard::vm_resolve` walks a `name[i]/leaf` path to drive a **list item** (which the flat resolver can't index); the `RiveViewModel` component accepts the same `[i]` paths in both tiers; **WRITE forwarding in BOTH tiers** ✅ (`floor` inline; `zero_copy` ferried to the render world before advance). `RiveViewModel` component = queued writes + typed `watch` read-back (floor). **Deferred:** zero-copy *watch* read-back (needs a render→main channel; floor reads cover the single-face case), list **structural** mutation (add/remove/swap), image/artboard ref props (blocked — see backlog) |
| State-machine inputs (bool/number/trigger) | ⛔ | [state-machines](https://rive.app/docs/runtimes/state-machines) | **Deprecated — not supported.** The classic `Scene::getBool/getNumber/getTrigger` path is superseded by view-model **data binding** (the modern channel, already shipped). See Excluded. |
| View-model change / trigger observation | 🟡 | [data-binding](https://rive.app/docs/runtimes/data-binding) | the **read** channel (modern *events* replacement): after advance, `flushChanges()` per watched path → `RiveViewModel::observe(path)` emits a `RivePropertyChanged` Bevy message when the rig fires a trigger or changes a property ✅ (floor). Supersedes the deprecated events read-back below. **Deferred:** zero-copy observe (render→main back-channel, like watch read-back). |
| ~~Events read-back (state changes, custom / open-url / audio)~~ | ⛔ | [state-machines](https://rive.app/docs/runtimes/state-machines) | **Deprecated by Rive — not supported.** "Listening to Rive Events at runtime is deprecated and will be removed in future versions." Use **view-model change / trigger observation** (the row above) instead. See Excluded. |
| Named artboard / state-machine selection | ✅ | [artboards](https://rive.app/docs/runtimes/artboards) | `ArtboardSelector` / `StateMachineSelector` honor **Default / ByName / ByIndex** in BOTH tiers (`File::artboard_named/_at`, `Artboard::state_machine_named/_at`); discover names via `artboard_names()` / `state_machine_names()` |
| Runtime text value get/set | ✅ | — | `TextValueRun` get/set by authored name (top-level or a nested artboard via a `/`-path). **`RiveText`** component queues set-writes (both tiers — `floor` inline, `zero_copy` ferried like view-model writes); `Artboard::text_get/text_set/text_set_in/text_run_names` at the safe layer. Setting re-shapes on the next advance. Bevy read-back deferred (safe-layer `text_get` covers it) |
| Out-of-band asset loading (images/fonts/audio) | ✅ | [loading-assets](https://rive.app/docs/runtimes/loading-assets) | `FileAssetLoader` callback → supply the **Referenced** (not Embedded) images / fonts / audio a `.riv` needs. **`RiveAssets`** component (name → encoded bytes), both tiers; `Context::load_file_with_assets` at the safe layer. Host returns encoded file bytes (PNG/JPEG/WEBP, font, audio); rive decodes via the context factory (libpng/jpeg/webp + harfbuzz). A name not in the map (or decode failure) falls back to in-band content |
| Audio playback | ✅ | [audio-events](https://rive.app/docs/runtimes/audio) | **system mode (default):** `--with_rive_audio=system` — rive owns a miniaudio device that plays a `.riv`'s audio events / embedded audio straight to the OS output **automatically during advance** (both tiers; no per-sound API). Host bridge controls: `rive_renderer::audio::{is_available,start,stop,set_volume}` (process-global engine) + the optional **`RiveAudio`** Bevy resource (master volume / mute). **host-mixer (external) mode:** the **`audio-external`** feature (`--with_rive_audio=external`) — rive owns NO device; the host pulls the mixed PCM (`rive_renderer::audio::external::{channels,sample_rate,read_frames,sum_frames}`) into its own mixer. `bevy-rive` routes it into **Bevy's own audio graph** via the **`RiveAudioStream`** `Decodable` source + **`RiveExternalAudioPlugin`** (unified mixing under Bevy's `GlobalVolume`; `RiveAudio` still applies as rive's master gain). The two modes are a mutually-exclusive whole-build choice |
| Joystick / gamepad / keyboard / focus input | 🔜 | — | `Scene` gamepad/keyboard + `FocusManager`; for game-controlled rigs |
| Animation playback controls (seek / pause / per-anim speed) | 🔜 | — | direct `LinearAnimationInstance` time control beyond the SM |
| Bones / constraints / solo runtime control | 🔜 | — | drive bones, toggle solo children, set constraint strength at runtime |

---

## Priority backlog (next features, ROI-ordered)

1. **Animation playback controls** (seek / pause / per-anim speed); **bones / constraints /
   solo runtime control**.

*(Recently shipped: **view-model per-item / list-item writes** — the `RiveViewModelInstance`
handle is now read-write (`set_*` / `fire_trigger`) and `Artboard::vm_resolve` walks a `name[i]/leaf`
path to drive a list item or nested VM (both tiers); **audio host-mixer routing** — the `audio-external`
feature (`--with_rive_audio=external`): rive owns no device, the host pulls the mixed PCM, and
`bevy-rive` routes it into Bevy's audio graph via `RiveAudioStream` / `RiveExternalAudioPlugin`;
**audio playback** — `--with_rive_audio=system`, rive plays audio events
to the OS output during advance + `RiveAudio` volume/mute; **atlas-tile pointer mapping** —
zero-copy atlas faces now forward pointer input with tile-aware inversion; **runtime text
get/set** — `TextValueRun` / `RiveText`, both tiers; **out-of-band asset loading** —
`FileAssetLoader` / `RiveAssets`, both tiers; view-model **change/trigger observation**.)*

(State-machine **inputs** AND **events read-back** are intentionally **out of scope** —
both deprecated; view-model data binding is the modern write *and* read channel. See Excluded.)

**View-model data binding — what's deferred (lower-value or blocked, not on the critical path):**
- **zero-copy watch read-back** — writes forward in both tiers; *reads* (watch) are floor-only.
  Needs a render→main back-channel (`zero_copy` advances in the render world); deferred because you
  rarely read back N atlas faces and floor reads cover the single-face case.
- **image / artboard reference props** — `propertyImage`/`propertyArtboard` are **set-only**. Out-of-band
  **asset loading** now ships (a referenced image can be supplied at load), but *data-binding* an image
  property at runtime still needs a `RenderImage` handle exposed at the safe layer (decode → bindable
  value); `propertyArtboard` needs a `BindableArtboard` from **nested-artboard binding**. Wire each WITH
  its value source, not before (a setter with nothing to pass it is a dead end). *(Likely next slice — the
  image half is now unblocked by asset loading.)*
- **list STRUCTURAL mutation (add/remove/swap)** — list read / size / item-introspection **and per-item
  writes** now ship (the handle is read-write; `vm_resolve` drives `list[i]/leaf`); only add/remove/swap +
  creating new list-item instances remain (need `ViewModelInstanceListRuntime` mutators + VM-instance
  construction). Reads + per-item writes cover the common game case.

---

## Excluded (obsolete / not-applicable)

| Item | Why |
|------|-----|
| Low-level `LinearAnimationInstance` direct playback | Superseded by state machines as the playback unit; we fall back to the default scene only when no SM exists. (Seek/pause control may still be added under "playback controls".) |
| State-machine inputs (`Scene::getBool/getNumber/getTrigger`) | **Deprecated.** The classic input path is superseded by view-model **data binding** (shipped: number/bool/trigger/color/string/enum get/set). Not supported by project decision. |
| Events read-back (`reportedEvent*` / `stateChanged*` runtime listening) | **Deprecated by Rive** (per [feature-support](https://rive.app/docs/feature-support)): "Listening to Rive Events at runtime is deprecated and will be removed in future versions. Use Data Binding to listen for triggers or changes to properties instead." Replaced by **view-model change / trigger observation** (`flushChanges`). Audio events still play automatically during advance (rendering feature); open-url/custom signals come through data binding. |
| `CommandQueue` / `CommandServer` | An *alternative* thread-decoupled API; we drive rive directly from the render-adjacent main thread (NonSend), so it is redundant, not additive. |
| `WITH_RIVE_TOOLS` editor surface | Editor/tooling mode that alters core runtime behavior (blanks rendering); never enabled in a playback runtime. |
| Deprecated `Factory` paths (e.g. `makeEmptyRenderPath`) | Legacy; the current `RenderContext` factory path is used. |
| Test/`#ifdef TESTING` hooks | Not part of the shipping runtime. |
