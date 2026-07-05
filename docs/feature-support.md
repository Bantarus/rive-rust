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
| Meshes / vertex deform, bones / skinning | ✅ | — | rendered; **runtime bone control** ✅ (rotation/scale/length + root x/y — see Bones / constraints / solo below) |
| Constraints (IK, distance, follow-path, transform, …) | ✅ | — | solved during advance; **runtime strength control** ✅ + **type-specific props** ✅ (IK invert/parentBoneCount, distance distance/mode, follow-path distance/orient/offset — see Bones / constraints / solo below) |
| Layout engine (Yoga flex), N-slice (9-patch), follow-path | ✅ | — | solved during advance; resize via target size |
| Solo (exclusive visibility) | ✅ | — | rendered; **runtime active-child toggle** ✅ (see Bones / constraints / solo below) |
| Text rendering (runs, modifiers, styles, text-follow-path) | ✅ | — | renders embedded text; **runtime text get/set** 🔜 |
| Nested artboards / artboard lists | ✅ | [artboards](https://rive.app/docs/runtimes/artboards) | rendered; **per-child runtime access** ✅ — `Artboard::nested_artboard_count`/`nested_artboard_names` introspect, `nested_artboard`(name) / `nested_artboard_at`(index, for unnamed components) / `nested_artboard_at_path`("a/b") return a **borrowed child `Artboard`** the SAME rig/text/joystick/solo/constraint setters drive (the child is auto-advanced by the parent; the handle keeps the parent alive via an `Rc`). The `RiveNestedTarget` Bevy component redirects an entity's `RiveRig`/`RiveText` writes to a child in BOTH tiers. **Artboard-reference data binding** (`propertyArtboard`) ✅ — `File::bindable_artboard_named`/`_default` → a `BindableArtboard`, bound via `Artboard::vm_set_artboard` / `RiveViewModelInstance::set_artboard` (both mirror the image-ref path; same-context enforced). Bevy `set_artboard` ferry deferred (no asset authors it; the per-instance `File` is dropped) |
| Scripting — autonomous nodes (e.g. BallBreath) | ✅ | — | needs `--with_rive_scripting` + a **Publish-signed** `.riv` + the shim VM bind (shipped) |
| Embedded image / font assets | ✅ | [loading-assets](https://rive.app/docs/runtimes/loading-assets) | in-band assets decode automatically |

## Runtime control & data (need an FFI/API)

| Feature | Status | Reference | Notes |
|---------|:------:|-----------|-------|
| Advance / playback tick | ✅ | [state-machines](https://rive.app/docs/runtimes/state-machines) | `StateMachine::advance`; per-instance `RiveAnimation.speed` / `paused` / `seek` (see playback controls below) |
| Pointer input → Listeners / joysticks | ✅ | [state-machines](https://rive.app/docs/runtimes/state-machines) | move/down/up/exit; `RivePointer` — **both tiers AND every zero-copy draw path** (floor + zero-copy **dedicated** + zero-copy **atlas** tiles). The inversion tracks the face's Fit/Alignment; atlas faces are tile-aware (target-pixel coords are normalized into the face's tile before inverting, via `set_pointer_tile`) |
| **View-model data binding** | 🟡 | [data-binding](https://rive.app/docs/runtimes/data-binding) | get/set **number/bool/trigger/color/string/enum** (flat + `/`-nested paths) ✅; **introspection incl. nested VMs + lists** via the borrowed `RiveViewModelInstance` handle (`Artboard::vm_root` → `view_model`/`list_size`/`list_item` + reads) ✅; **per-item / nested writes** ✅ — the handle is now read-**write** (`set_*` / `fire_trigger`), and `Artboard::vm_resolve` walks a `name[i]/leaf` path to drive a **list item** (which the flat resolver can't index); the `RiveViewModel` component accepts the same `[i]` paths in both tiers; **WRITE forwarding in BOTH tiers** ✅ (`floor` inline; `zero_copy` ferried to the render world before advance). **Image-reference props** ✅ — decode encoded bytes (PNG/JPEG/WEBP) to a reusable `RiveImage` (`Context::decode_image`), then bind with `Artboard::vm_set_image` (flat + `/`-nested) / `RiveViewModelInstance::set_image` (nested/list-item), or unbind with `vm_clear_image`/`clear_image`; `RiveViewModel::set_image(path, bytes)` ferries + decodes at apply in BOTH tiers (same-context enforced → `ContextMismatch`). `RiveViewModel` component = queued writes + typed `watch` read-back in **BOTH tiers** ✅ — `floor` refreshes `values` inline after advance (same frame); `zero_copy` reads after the render-node advance and ships results back over the **render→main back-channel** (`RiveReadbackChannel`, drained in `PreUpdate` — so a `zero_copy` read-back trails the advance it observed by one frame). **Artboard-reference props** ✅ — `File::bindable_artboard_named`/`_default` build a `BindableArtboard`, bound via `Artboard::vm_set_artboard` / `RiveViewModelInstance::set_artboard` (mirrors the image-ref path; same-context enforced); see the nested-artboard row. **Deferred:** list **structural** mutation (add/remove/swap), Bevy `set_artboard` ferry (safe-layer-only; the per-instance `File` is dropped, and no demo asset authors `propertyArtboard`) |
| State-machine inputs (bool/number/trigger) | ⛔ | [state-machines](https://rive.app/docs/runtimes/state-machines) | **Deprecated — not supported.** The classic `Scene::getBool/getNumber/getTrigger` path is superseded by view-model **data binding** (the modern channel, already shipped). See Excluded. |
| View-model change / trigger observation | ✅ | [data-binding](https://rive.app/docs/runtimes/data-binding) | the **read** channel (modern *events* replacement): after advance, `flushChanges()` per watched path → `RiveViewModel::observe(path)` emits a `RivePropertyChanged` Bevy message when the rig fires a trigger or changes a property — **BOTH tiers** (`floor` emits inline; `zero_copy` fires travel the render→main back-channel and are emitted from the `PreUpdate` drain, one frame after the advance that fired; both zero-copy advance paths — dedicated + atlas — are wired). Supersedes the deprecated events read-back below. |
| ~~Events read-back (state changes, custom / open-url / audio)~~ | ⛔ | [state-machines](https://rive.app/docs/runtimes/state-machines) | **Deprecated by Rive — not supported.** "Listening to Rive Events at runtime is deprecated and will be removed in future versions." Use **view-model change / trigger observation** (the row above) instead. See Excluded. |
| Named artboard / state-machine selection | ✅ | [artboards](https://rive.app/docs/runtimes/artboards) | `ArtboardSelector` / `StateMachineSelector` honor **Default / ByName / ByIndex** in BOTH tiers (`File::artboard_named/_at`, `Artboard::state_machine_named/_at`); discover names via `artboard_names()` / `state_machine_names()` |
| Runtime text value get/set | ✅ | — | `TextValueRun` get/set by authored name (top-level or a nested artboard via a `/`-path). **`RiveText`** component queues set-writes (both tiers — `floor` inline, `zero_copy` ferried like view-model writes); `Artboard::text_get/text_set/text_set_in/text_run_names` at the safe layer. Setting re-shapes on the next advance. Bevy read-back deferred (safe-layer `text_get` covers it) |
| Out-of-band asset loading (images/fonts/audio) | ✅ | [loading-assets](https://rive.app/docs/runtimes/loading-assets) | `FileAssetLoader` callback → supply the **Referenced** (not Embedded) images / fonts / audio a `.riv` needs. **`RiveAssets`** component (name → encoded bytes), both tiers; `Context::load_file_with_assets` at the safe layer. Host returns encoded file bytes (PNG/JPEG/WEBP, font, audio); rive decodes via the context factory (libpng/jpeg/webp + harfbuzz). A name not in the map (or decode failure) falls back to in-band content |
| Audio playback | ✅ | [audio-events](https://rive.app/docs/runtimes/audio) | **system mode (default):** `--with_rive_audio=system` — rive owns a miniaudio device that plays a `.riv`'s audio events / embedded audio straight to the OS output **automatically during advance** (both tiers; no per-sound API). Host bridge controls: `rive_renderer::audio::{is_available,start,stop,set_volume}` (process-global engine) + the optional **`RiveAudio`** Bevy resource (master volume / mute). **host-mixer (external) mode:** the **`audio-external`** feature (`--with_rive_audio=external`) — rive owns NO device; the host pulls the mixed PCM (`rive_renderer::audio::external::{channels,sample_rate,read_frames,sum_frames}`) into its own mixer. `bevy-rive` routes it into **Bevy's own audio graph** via the **`RiveAudioStream`** `Decodable` source + **`RiveExternalAudioPlugin`** (unified mixing under Bevy's `GlobalVolume`; `RiveAudio` still applies as rive's master gain). The two modes are a mutually-exclusive whole-build choice |
| **Joystick / gamepad / keyboard / focus input** | ✅ | — | host-driven inputs for game-controlled rigs, both tiers, via the **`RiveInput`** component (queues commands applied before advance — `floor` inline; `zero_copy` ferried, like the rig writes). Two shapes: **Joystick** is an AUTHORED component (like a bone) — `RiveInput::set_joystick(name, x, y)` / `Artboard::joystick_set`/`joystick_get`/`joystick_names`; the artboard APPLIES it during advance (drives linked animations/constraints), so a set sticks unless an animation also keys it (render-proven: the eye-joysticks demo's "Pupil" joystick visibly moves). **Keyboard / gamepad / focus** are a state-machine EVENT feed routed through the SM's `FocusManager` (focus tree auto-built at SM creation) to the focused element's listeners — `StateMachine::key_input(Key, KeyModifiers, …)` / `text_input` / `gamepad_button(GamepadButton, …)` / `gamepad_axis(GamepadAxis, …)` / `focus_advance(FocusDir)` / `clear_focus` / `focus_state()`; `RiveInput::key`/`key_down`/`key_up`/`text`/`gamepad_button`/`gamepad_axis`/`focus`/`clear_focus`. The event feed only DOES something when the `.riv` authors `FocusData` + key/gamepad listeners (otherwise "not consumed" — no demo asset authors these yet, so the feed is API/compile-verified, not render-proven). Gamepad state accumulates a faithful W3C snapshot per SM. **Bevy-side `focus_state` read-back** ✅ — `RiveInput::watch_focus()` + `focus_state()` (BOTH tiers: `floor` reads inline after advance; `zero_copy` over the render→main channel, one frame late; API-proven — the default nothing-focused state delivers; no demo asset authors `FocusData`). **Deferred:** multi-device gamepads; raw (non-standard) gamepad indices in the typed API |
| **Animation playback controls (seek / pause / per-anim speed)** | ✅ | [state-machines](https://rive.app/docs/runtimes/state-machines) | **per-instance speed** ✅ (`RiveAnimation.speed` — a `Time::delta` multiplier, both tiers; rive has no native per-animation speed setter, so this is the dt lever) + **pause** ✅ (`RiveAnimation.paused` / `pause()`/`resume()` — advances by 0 so time freezes but the frame still renders and data binding still applies; distinct from `RiveActive(false)`, which *culls*; both tiers) + **seek** ✅ — `StateMachine::seek(t)` / `duration()` / `time()` at the safe layer, and the `RiveAnimation::seek(t)` one-shot in BOTH tiers (`floor` drains inline; `zero_copy` stages → ferries → applies before advance, like view-model writes). Seek applies immediately (visible while paused = scrubbing); times clamp to `[0, duration]`. **Only linear-animation scenes are seekable** (the default-scene animation fallback when an artboard has no state machine) — a seek on a state machine returns `false` / no-ops (no scalar playhead); `duration()`/`time()` return `None` there. **Bevy-side playhead read-back** ✅ — `RiveAnimation::watch_playhead()` + `playhead()`/`duration()` (BOTH tiers: `floor` same-frame, `zero_copy` one frame late over the render→main channel; live-proven on the eye-joysticks 3s linear scene — advances + wraps, duration `Some(3.0)`; a state machine reads `None`/`None` as documented). **Deferred:** state-machine `reset()`; per-animation (not per-instance) speed |
| **Bones / constraints / solo runtime control** | ✅ | — | drive a rig by AUTHORED component name (`ArtboardInstance::find<T>`), universal knobs, both tiers. **Bones** — `Artboard::bone_get/bone_set(name, BoneProp, f32)`: rotation/scaleX/scaleY/length on any bone, x/y on **root bones only** (regular-bone x/y are derived → `Error::Rig`). **Constraints** — `Artboard::constraint_get_strength`/`constraint_set_strength(name, f32)` (every constraint has strength), plus **type-specific props** `constraint_get_prop`/`constraint_set_prop(name, ConstraintProp, f32)` — IK `invert`/`parentBoneCount`, distance `distance`/`mode` (Closer/Further/Exact), follow-path `distance`/`orient`/`offset` (bools ride the f32 channel as 0/1; addressing a prop on a constraint of the wrong type → `Error::Rig`). **Solo** — `Artboard::solo_set_active(name, child)` by name / `solo_set_active_index(name, i)`, read via `solo_get_active`/`solo_get_active_index`. **`RiveRig`** component queues writes applied before advance in BOTH tiers (`floor` inline; `zero_copy` staged → ferried → applied before advance, like text/view-model writes). A write takes effect on the next advance and **sticks only if the active animation doesn't also key that property** (advance solves on top — re-queue each frame for procedural control). The `RiveRig` component also exposes `set_constraint_prop(name, ConstraintProp, value)` for the type-specific props (both tiers, ferried free with the other rig writes). Introspection: `bone_names`/`constraint_names`/`solo_names`. **Bevy-side rig read-back** ✅ — register with `RiveRig::watch_bone`/`watch_constraint_strength`/`watch_constraint_prop`/`watch_solo`, read with `bone`/`constraint_strength`/`constraint_prop`/`solo_active`(`_index`) (BOTH tiers: `floor` refreshes inline after advance, `zero_copy` over the render→main channel one frame late; reads honor the same `RiveNestedTarget` redirect as the rig writes; live-proven — a procedurally spun bone's read tracks the write +3°/frame). **Deferred:** transform-constraint origin, list of bone children |

---

## Priority backlog (next features, ROI-ordered)

1. **List structural mutation + nested-VM construction** — list read / size / per-item writes ship;
   add/remove/swap + creating new list-item / VM instances remain (need `ViewModelInstanceListRuntime`
   mutators). Pairs with the Bevy `set_artboard` ferry (needs per-instance `File` retention).

*(Backlog item "Bevy-side read-back in `zero_copy`" is COMPLETE: the render→main back-channel
(`RiveReadbackChannel`, an `Arc<Mutex<Vec>>` shared by both worlds — node reads after advance,
`PreUpdate` drain fans out, one frame of latency vs floor's same-frame reads) carries all the
backlog read surfaces: view-model watch + observe, rig (bone/constraint/solo), `focus_state`,
and the live playhead/duration — see the per-feature rows above. Text-run read-back remains
safe-layer-only, a candidate for the same channel.)*

*(Recently shipped: **nested-artboard runtime access + artboard-reference binding** — reach into a
child artboard mounted by a `NestedArtboard` component: `Artboard::nested_artboard_count`/`_names`
introspect, `nested_artboard`(name) / `nested_artboard_at`(index — needed because designers leave the
components unnamed) / `nested_artboard_at_path`("a/b") return a **borrowed child `Artboard`** that the
SAME rig/text/joystick/solo/constraint setters drive (the child is auto-advanced by the parent; the
handle keeps the parent alive via an `Rc`, so no lifetime param). Render-proven on the slot-machine
demo (driving a nested body bone visibly tilts it, deterministic + value-sensitive). The
`RiveNestedTarget` Bevy component redirects an entity's `RiveRig`/`RiveText` writes to a child in BOTH
tiers. **Artboard-reference data binding** (`propertyArtboard`, the last deferred VM property) —
`File::bindable_artboard_named`/`_default` build a `BindableArtboard` bound via `Artboard::vm_set_artboard`
/ `RiveViewModelInstance::set_artboard` (mirror the image-ref path; same-context enforced); API/round-trip-
verified (no demo asset authors `propertyArtboard`, like the focus case). **joystick / gamepad / keyboard / focus input** — host-driven inputs for
game-controlled rigs, both tiers, via the `RiveInput` component. **Joystick** is an authored
component (like a bone) — `set_joystick(name, x, y)` / `Artboard::joystick_set`/`joystick_get`/
`joystick_names`, applied during advance (render-proven on the eye-joysticks demo). **Keyboard /
gamepad / focus** are a state-machine event feed via the SM's `FocusManager` —
`StateMachine::key_input`/`text_input`/`gamepad_button`/`gamepad_axis`/`focus_advance`/`clear_focus`/
`focus_state`; only active when the `.riv` authors `FocusData` + listeners (API/compile-verified,
no demo asset authors these). **bones / constraints / solo runtime control** — drive a rig by authored
component name (`find<T>`), universal knobs, both tiers: `Artboard::bone_get/bone_set`
(rotation/scale/length any bone, x/y root bones only), `constraint_get_strength`/`constraint_set_strength`,
`solo_set_active`/`solo_set_active_index`; the `RiveRig` component queues writes applied before advance
in both tiers (a write sticks only if the animation doesn't also key that property); introspection via
`bone_names`/`constraint_names`/`solo_names`; **animation playback controls** — per-instance `RiveAnimation.speed` /
`paused` (advance-by-0 freeze; both tiers) + **seek** via `StateMachine::seek`/`duration`/`time`
(safe layer) and the `RiveAnimation::seek(t)` one-shot (both tiers); linear-animation scenes only
(state machines have no scalar playhead); **view-model image-reference data binding** — `Context::decode_image` turns
encoded bytes (PNG/JPEG/WEBP) into a reusable `RiveImage`, bound to an image property via
`Artboard::vm_set_image` / `RiveViewModelInstance::set_image` / `RiveViewModel::set_image` (both tiers;
same-context enforced); **view-model per-item / list-item writes** — the `RiveViewModelInstance`
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
- **zero-copy watch read-back — now SHIPS** (the render→main back-channel: the node reads
  watches / flushes observes after advance — both the dedicated and atlas paths — and a
  `PreUpdate` drain writes `RiveViewModel::values` + emits `RivePropertyChanged`; one frame
  of latency vs floor's same-frame reads). The same channel now also carries the rig /
  `focus_state` / playhead reads (see their rows); text-run read-back remains safe-layer-only.
- **artboard reference props** — both now **ship**: `propertyImage` (decode → `RiveImage` →
  `vm_set_image`) and `propertyArtboard` (`File::bindable_artboard_*` → `BindableArtboard` →
  `vm_set_artboard` / `set_artboard`), shipped WITH nested-artboard access (which is where the
  `BindableArtboard` value source comes from). The only remaining gap is the **Bevy `set_artboard`
  ferry** — safe-layer + example only, since the per-instance `File` is dropped after build and no
  demo asset authors a `propertyArtboard` property to render-prove it (the focus/FocusData situation).
- **list STRUCTURAL mutation (add/remove/swap)** — list read / size / item-introspection **and per-item
  writes** now ship (the handle is read-write; `vm_resolve` drives `list[i]/leaf`); only add/remove/swap +
  creating new list-item instances remain (need `ViewModelInstanceListRuntime` mutators + VM-instance
  construction). Reads + per-item writes cover the common game case.

---

## Excluded (obsolete / not-applicable)

| Item | Why |
|------|-----|
| Low-level `LinearAnimationInstance` direct playback | Superseded by state machines as the playback unit; we fall back to the default scene only when no SM exists. (Seek / pause / speed control on that default scene **now ships** — see "Animation playback controls" above; what stays excluded is *instantiating arbitrary named animations* as independent playback units.) |
| State-machine inputs (`Scene::getBool/getNumber/getTrigger`) | **Deprecated.** The classic input path is superseded by view-model **data binding** (shipped: number/bool/trigger/color/string/enum get/set). Not supported by project decision. |
| Events read-back (`reportedEvent*` / `stateChanged*` runtime listening) | **Deprecated by Rive** (per [feature-support](https://rive.app/docs/feature-support)): "Listening to Rive Events at runtime is deprecated and will be removed in future versions. Use Data Binding to listen for triggers or changes to properties instead." Replaced by **view-model change / trigger observation** (`flushChanges`). Audio events still play automatically during advance (rendering feature); open-url/custom signals come through data binding. |
| `CommandQueue` / `CommandServer` | An *alternative* thread-decoupled API; we drive rive directly from the render-adjacent main thread (NonSend), so it is redundant, not additive. |
| `WITH_RIVE_TOOLS` editor surface | Editor/tooling mode that alters core runtime behavior (blanks rendering); never enabled in a playback runtime. |
| Deprecated `Factory` paths (e.g. `makeEmptyRenderPath`) | Legacy; the current `RenderContext` factory path is used. |
| Test/`#ifdef TESTING` hooks | Not part of the shipping runtime. |
