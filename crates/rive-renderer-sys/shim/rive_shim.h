/*
 * rive_shim.h — minimal C ABI over the native Rive Renderer (rive-runtime PLS,
 * Vulkan backend), for Milestone 0: render a .riv offscreen and read pixels back.
 *
 * This ABI is adapted from the project's original RiveSharp-style sketch to the
 * REAL rive-runtime API. Notable, deliberate deviations from the sketch (each is
 * a consequence of the real source; see BUILD.md "C ABI deviations"):
 *
 *   - The context is created by `rive_render_context_create_vulkan_self`, which
 *     uses rive's own `rive_vk_bootstrap` (compiled INTO this shim) to create a
 *     headless VkInstance/VkPhysicalDevice/VkDevice + graphics queue, then calls
 *     `rive::gpu::RenderContextVulkanImpl::MakeContext(...)`.
 *   - An offscreen `RiveRenderTarget` bundles rive's `RenderTargetVulkanImpl`
 *     with a `rive_vkb::VulkanHeadlessFrameSynchronizer` (the offscreen image,
 *     per-frame command buffer, fence, and CPU readback all live there).
 *   - `rive_file_load` imports via `rive::File::import`, passing the RenderContext
 *     itself AS the `rive::Factory` (RenderContext IS-A Factory).
 *   - "state machine" is backed by a `rive::Scene` (default state machine if the
 *     designer set one, else `defaultScene()`); `advance` calls advanceAndApply.
 *   - `rive_frame_begin/_draw/_flush` map onto beginFrame / RiveRenderer +
 *     artboard->draw / flush(FlushResources) + queueImageCopy + endFrame +
 *     getPixelsFromLastImageCopy. Drawing fits the artboard with Fit::contain +
 *     Alignment::center.
 *   - Pixel readback yields PREMULTIPLIED, top-down, sRGB-encoded RGBA8
 *     (VK_FORMAT_R8G8B8A8_UNORM). The caller un-premultiplies for a viewer.
 *
 * Error model: constructors return NULL on failure; verbs return a `RiveStatus`
 * (0 == success). No C++ exceptions cross this boundary (rive is built with
 * exceptions off, and the shim catches at the boundary). `rive_last_error`
 * returns a human-readable description of the most recent failure (M0: a single
 * global, not thread-safe).
 */
#ifndef RIVE_SHIM_H
#define RIVE_SHIM_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct RiveRenderContext RiveRenderContext;
typedef struct RiveRenderTarget  RiveRenderTarget;
typedef struct RiveFile          RiveFile;
typedef struct RiveArtboard      RiveArtboard;
typedef struct RiveStateMachine  RiveStateMachine;
/* A view-model INSTANCE (the artboard's root VM, a nested VM, or a list item).
 * Borrowed: aliases an instance owned by rive's caches under the root view model,
 * so it is valid only while the owning RiveArtboard lives (and, for list items,
 * while the addressed list is unmodified). Obtained from rive_artboard_vm_root /
 * rive_vmi_property_view_model / rive_vmi_list_instance_at; never freed by Rust. */
typedef struct RiveViewModelInstance RiveViewModelInstance;

/* An OWNED, decoded render image — the value source for image-property data
 * binding (rive_artboard_vm_set_image / rive_vmi_set_image). Created by
 * rive_image_decode and freed with rive_image_destroy. Bound to the render
 * context it was decoded with; bind only into artboards on that same context. */
typedef struct RiveImage RiveImage;

/* An OWNED, file-sourced artboard value — the value source for artboard-reference
 * (propertyArtboard) data binding (rive_artboard_vm_set_artboard / rive_vmi_set_artboard),
 * the artboard analogue of RiveImage. Created by rive_file_bindable_artboard_named /
 * _default and freed with rive_bindable_artboard_destroy. */
typedef struct RiveBindableArtboard RiveBindableArtboard;

/* 0 == success; nonzero == failure (see rive_last_error). */
typedef int32_t RiveStatus;
#define RIVE_OK 0

/* Returns a static, human-readable description of the most recent failure, or
 * an empty string if none. Valid until the next failing shim call. */
const char* rive_last_error(void);

/* --- Context (M0: shim creates and owns its own VkInstance/VkDevice) ------- */

/* Creates a headless Vulkan device and a native Rive RenderContext on it.
 * Honors env vars: RIVE_GPU (substring GPU-name filter; "integrated" picks an
 * integrated GPU) and RIVE_FORCE_ATOMIC (if set, forces the atomic PLS path).
 * Returns NULL on failure. */
RiveRenderContext* rive_render_context_create_vulkan_self(void);
void               rive_render_context_destroy(RiveRenderContext*);

/* --- Offscreen render target (rive render target + headless synchronizer) -- */

RiveRenderTarget*  rive_render_target_create_offscreen(RiveRenderContext*,
                                                       uint32_t width,
                                                       uint32_t height);
void               rive_render_target_destroy(RiveRenderTarget*);
uint32_t           rive_render_target_width(const RiveRenderTarget*);
uint32_t           rive_render_target_height(const RiveRenderTarget*);
/* Size in bytes of the RGBA8 readback buffer == width * height * 4. */
size_t             rive_render_target_pixel_buffer_size(const RiveRenderTarget*);

/* --- File / artboard / state machine --------------------------------------- */

/* Imports a .riv from memory using `ctx` as the rive::Factory. The bytes are
 * only borrowed for the duration of the call. Returns NULL on failure. */
RiveFile*          rive_file_load(RiveRenderContext* ctx,
                                  const uint8_t* bytes,
                                  size_t len);
void               rive_file_destroy(RiveFile*);

/* --- Out-of-band asset loading --------------------------------------------- */

/* Asset kind reported to the loader callback (rive's FileAsset subtype). */
#define RIVE_ASSET_OTHER 0
#define RIVE_ASSET_IMAGE 1
#define RIVE_ASSET_FONT  2
#define RIVE_ASSET_AUDIO 3

/* Describes one asset the file references, passed by const-pointer to the host
 * loader callback. The `const char*` fields are NUL-terminated and valid only
 * for the duration of the callback; `in_band_bytes` is the asset's embedded
 * content (NULL/0 when the asset is referenced out-of-band, i.e. exported as
 * "Referenced" rather than "Embedded"). */
typedef struct RiveAssetRequest {
    const char*    name;           /* authored asset name, e.g. "logo.png" */
    const char*    file_extension; /* lowercase, no dot, e.g. "png", "ttf"  */
    const char*    cdn_uuid;       /* CDN UUID string, or "" if none         */
    uint32_t       asset_id;       /* file-unique asset id                   */
    uint16_t       asset_type;     /* one of RIVE_ASSET_* above              */
    const uint8_t* in_band_bytes;  /* embedded content, or NULL              */
    size_t         in_band_len;    /* length of in_band_bytes, or 0          */
} RiveAssetRequest;

/* Host loader callback, invoked synchronously once per referenced asset during
 * rive_file_load_with_assets. To supply an asset, point *out_bytes/*out_len at
 * ENCODED file bytes (a PNG/JPEG/WEBP image, or a font/audio file) and return 1;
 * rive decodes them via the render context's factory. The buffer need only stay
 * valid until the callback returns (the shim copies it immediately). Return 0 to
 * decline, letting rive fall back to the in-band content (if any). `user` is the
 * opaque pointer passed to rive_file_load_with_assets. */
typedef int (*RiveAssetLoadFn)(void* user,
                               const RiveAssetRequest* req,
                               const uint8_t** out_bytes,
                               size_t* out_len);

/* Like rive_file_load, but installs `load_fn` as the file's out-of-band asset
 * loader (called synchronously for each referenced asset during this call;
 * neither `load_fn` nor `user` is retained afterwards). `load_fn == NULL`
 * behaves exactly like rive_file_load. */
RiveFile*          rive_file_load_with_assets(RiveRenderContext* ctx,
                                              const uint8_t* bytes,
                                              size_t len,
                                              RiveAssetLoadFn load_fn,
                                              void* user);

/* --- Image decode (value source for image-property data binding) ------------ */

/* Decodes ENCODED image bytes (PNG/JPEG/WEBP) into an owned RiveImage via `ctx`'s
 * factory. The result is tied to `ctx`'s device — bind it only into artboards on
 * the same context. Returns NULL on bad arguments or a decode failure. */
RiveImage*         rive_image_decode(RiveRenderContext* ctx,
                                     const uint8_t* bytes, size_t len);
void               rive_image_destroy(RiveImage*);

/* Instantiates an artboard. `_default` uses the file's default; `_named` /`_at`
 * select by name / 0-based index. All bind the artboard's default view model
 * identically. Return NULL (+ rive_last_error) if the file is invalid or the
 * artboard isn't found. */
RiveArtboard*      rive_file_artboard_default(RiveFile*);
RiveArtboard*      rive_file_artboard_named(RiveFile*, const char* name);
RiveArtboard*      rive_file_artboard_at(RiveFile*, uint32_t index);
/* Selection introspection: discover the names a ByName/ByIndex selector can pick.
 * `name_at` uses the two-call buffer protocol (buf=NULL, cap=0 to size first;
 * bytes are NOT NUL-terminated). Returns nonzero on invalid handle / index. */
uint32_t           rive_file_artboard_count(RiveFile*);
RiveStatus         rive_file_artboard_name_at(RiveFile*, uint32_t index,
                                              char* buf, size_t cap, size_t* out_len);
void               rive_artboard_destroy(RiveArtboard*);

/* --- Nested-artboard runtime access -------------------------------------------
 * Reach into a child artboard mounted by a NestedArtboard component. `nested_count`
 * /`nested_name_at` list the NestedArtboard component names (two-call buffer
 * protocol; not NUL-terminated). `nested_named` resolves a child by that name;
 * `nested_at_path` resolves a '/'-delimited path ("child/grandchild"). Both return
 * a BORROWED RiveArtboard handle — the SAME bone/text/joystick/solo/constraint
 * functions then drive the child. Free it with rive_artboard_destroy; it is valid
 * only while the parent artboard lives. NULL (+ rive_last_error) if not found or the
 * nested artboard has no mounted instance. All accept a top-level OR a nested handle. */
uint32_t           rive_artboard_nested_count(RiveArtboard*);
RiveStatus         rive_artboard_nested_name_at(RiveArtboard*, uint32_t index,
                                                char* buf, size_t cap, size_t* out_len);
RiveArtboard*      rive_artboard_nested_at(RiveArtboard*, uint32_t index);
RiveArtboard*      rive_artboard_nested_named(RiveArtboard*, const char* name);
RiveArtboard*      rive_artboard_nested_at_path(RiveArtboard*, const char* path);

/* --- BindableArtboard value source (artboard-reference data binding) -----------
 * Create a bindable artboard value from this file by name / default (holding the
 * File alive); bind it to a propertyArtboard with rive_artboard_vm_set_artboard /
 * rive_vmi_set_artboard. Return NULL (+ rive_last_error) if the file is invalid or
 * the artboard isn't found. Free with rive_bindable_artboard_destroy (binding takes
 * its own ref, so it may be freed after binding). */
RiveBindableArtboard* rive_file_bindable_artboard_named(RiveFile*, const char* name);
RiveBindableArtboard* rive_file_bindable_artboard_default(RiveFile*);
void                  rive_bindable_artboard_destroy(RiveBindableArtboard*);

/* Instantiates a scene/state machine to play. `_default` prefers the designer
 * default state machine, then falls back to the default Scene (first state
 * machine, else first animation, else static). `_named` /`_at` select a state
 * machine by name / 0-based index — state-machine ONLY, no animation fallback
 * (a miss is an error, not a silent default). Return NULL (+ rive_last_error)
 * if nothing is playable / the name/index isn't found. */
RiveStateMachine*  rive_artboard_state_machine_default(RiveArtboard*);
RiveStateMachine*  rive_artboard_state_machine_named(RiveArtboard*, const char* name);
RiveStateMachine*  rive_artboard_state_machine_at(RiveArtboard*, uint32_t index);
/* Selection introspection (see the artboard pair above). */
uint32_t           rive_artboard_state_machine_count(RiveArtboard*);
RiveStatus         rive_artboard_state_machine_name_at(RiveArtboard*, uint32_t index,
                                                       char* buf, size_t cap, size_t* out_len);
void               rive_state_machine_destroy(RiveStateMachine*);

/* Advances the state machine (advanceAndApply) by `dt_seconds`, applying the
 * result to its backing artboard. */
void               rive_state_machine_advance(RiveStateMachine*, float dt_seconds);

/* --- Playback controls (seek / duration / time) ----------------------------
 * Seek/time work on a LINEAR-ANIMATION scene only (the default-scene fallback
 * when an artboard has no state machine). State machines have no scalar playhead:
 * _duration returns -1, _time returns -1, _seek returns false (no-op). Pause and
 * speed are NOT here — they are dt manipulation the caller already owns (pass 0 /
 * scaled dt to _advance). */
/* Animation length in seconds, or -1 for a state machine / null handle. */
float              rive_state_machine_duration(RiveStateMachine*);
/* Current playhead in seconds, or -1 for a state machine / null handle. */
float              rive_state_machine_time(RiveStateMachine*);
/* Seek to absolute time `t` (seconds, clamped to [0, duration]) and apply
 * immediately so the pose is visible without a following _advance. Returns true
 * if seekable (a linear-animation scene), false for a state machine / null. */
bool               rive_state_machine_seek(RiveStateMachine*, float t);

/* --- Fit / alignment (how the artboard maps into its draw target) -----------
 * Stored on the handle; rive_artboard_draw / _viewport read the artboard's, and
 * pointer inversion reads the state machine's (set BOTH to the same values, via
 * the RiveFit component, or pointer hits won't line up). `fit` is a Fit ordinal:
 * fill=0, contain=1, cover=2, fitWidth=3, fitHeight=4, none=5, scaleDown=6,
 * layout=7 (out-of-range -> contain). `align_x`/`align_y` are -1..1 (center=0,0;
 * bottomCenter=0,1). `scale_factor` applies only to Fit::layout. The default
 * (contain / center / 1.0) reproduces the historical hardcoded transform. */
void               rive_artboard_set_fit_align(RiveArtboard*, uint32_t fit,
                                               float align_x, float align_y,
                                               float scale_factor);
void               rive_state_machine_set_fit_align(RiveStateMachine*, uint32_t fit,
                                                    float align_x, float align_y,
                                                    float scale_factor);
/* Atlas pointer mapping: the DRAWN tile size (px) an atlas face renders into via
 * rive_artboard_draw_viewport. When > 0, the pointer fns normalize target-space
 * coords into this tile before inverting the fit/alignment (an atlas face is fit
 * into its tile, not the full target). Pass (0, 0) to restore full-target
 * inversion (dedicated faces — the default). Set per-frame by the atlas node. */
void               rive_state_machine_set_pointer_tile(RiveStateMachine*,
                                                       float tile_w, float tile_h);

/* --- View-model data binding (get/set named view-model properties) ----------
 * Operate on the artboard's bound DEFAULT view-model instance (see
 * data-binding.mdx). `path` is a UTF-8 property name; nested view models use a
 * '/' separator (e.g. "group/child/x"). Each verb returns RiveStatus (RIVE_OK,
 * else nonzero + rive_last_error) — nonzero on no-view-model / path-not-found /
 * wrong-type. Slice 1 = number/bool/trigger + schema introspection; color,
 * string and enum follow. Implemented in rive_shim_viewmodel.cpp. */
RiveStatus         rive_artboard_vm_set_number(RiveArtboard*, const char* path, float value);
RiveStatus         rive_artboard_vm_get_number(RiveArtboard*, const char* path, float* out);
RiveStatus         rive_artboard_vm_set_bool(RiveArtboard*, const char* path, uint8_t value);
RiveStatus         rive_artboard_vm_get_bool(RiveArtboard*, const char* path, uint8_t* out);
RiveStatus         rive_artboard_vm_fire_trigger(RiveArtboard*, const char* path);
/* Slice 2: color (ARGB u32), string, enum. Strings/enum-names use the two-call
 * buffer protocol (call with buf=NULL, cap=0 to get *out_len, then size + refill;
 * bytes are NOT NUL-terminated). Enum get/set by index or name; enumerate labels. */
RiveStatus         rive_artboard_vm_set_color(RiveArtboard*, const char* path, uint32_t argb);
RiveStatus         rive_artboard_vm_get_color(RiveArtboard*, const char* path, uint32_t* out);
RiveStatus         rive_artboard_vm_set_string(RiveArtboard*, const char* path, const char* value);
RiveStatus         rive_artboard_vm_get_string(RiveArtboard*, const char* path,
                                               char* buf, size_t cap, size_t* out_len);
RiveStatus         rive_artboard_vm_set_enum_index(RiveArtboard*, const char* path, uint32_t index);
RiveStatus         rive_artboard_vm_get_enum_index(RiveArtboard*, const char* path, uint32_t* out);
RiveStatus         rive_artboard_vm_set_enum_name(RiveArtboard*, const char* path, const char* name);
RiveStatus         rive_artboard_vm_enum_value_count(RiveArtboard*, const char* path, uint32_t* out);
RiveStatus         rive_artboard_vm_enum_value_at(RiveArtboard*, const char* path, uint32_t index,
                                                  char* buf, size_t cap, size_t* out_len);
/* Schema introspection (discover property names + types). `property_at` writes
 * up to `cap` name bytes (NOT NUL-terminated), always sets *out_len to the full
 * name length (call with cap=0 to size first), and *out_type to the rive
 * DataType ordinal (number=2, boolean=3, color=4, string=1, enum=6, trigger=7). */
uint32_t           rive_artboard_vm_property_count(RiveArtboard*);
RiveStatus         rive_artboard_vm_property_at(RiveArtboard*, uint32_t index,
                                                char* name_buf, size_t cap,
                                                size_t* out_len, int* out_type);

/* Change / trigger OBSERVATION (the modern, non-deprecated events replacement —
 * Rive deprecated runtime event listening; use data binding instead). Sets *out
 * to 1 if the property at `path` changed (or, for a trigger, FIRED) on the last
 * advance, else 0 — consuming the flag (next call returns 0 until it changes
 * again). Type-agnostic (any property type incl. trigger). Subscribe by calling
 * once BEFORE the first advance (prime), then poll each frame AFTER advance. */
RiveStatus         rive_artboard_vm_flush_changed(RiveArtboard*, const char* path, uint8_t* out);
/* Bind a decoded image to a root-VM image property (`/` reaches nested VMs); a
 * NULL image clears it. The image must be from the same context as the artboard. */
RiveStatus         rive_artboard_vm_set_image(RiveArtboard*, const char* path, RiveImage* image);
/* Bind a file-sourced BindableArtboard to a root-VM artboard property (`/` reaches
 * nested VMs); a NULL bindable clears it. See rive_file_bindable_artboard_*. */
RiveStatus         rive_artboard_vm_set_artboard(RiveArtboard*, const char* path,
                                                 RiveBindableArtboard* bindable);

/* --- View-model handle API (nested VMs + lists) -----------------------------
 * Operate on a RiveViewModelInstance* (root / nested / list item). The flat path
 * above reaches NAMED nested view models via '/', but cannot index lists nor
 * introspect a nested VM's schema; the handle API can. Navigation returns a
 * borrowed handle (null + rive_last_error on miss); reads/introspection mirror
 * the artboard-rooted verbs. Read-only this slice (writes via the setters above;
 * list mutation + image/artboard refs deferred). Implemented in
 * rive_shim_viewmodel.cpp. `out_type` ordinals add list=5, viewModel=8,
 * assetImage=11, artboard=12 to the scalar set documented above. */
RiveViewModelInstance* rive_artboard_vm_root(RiveArtboard*);
RiveViewModelInstance* rive_vmi_property_view_model(RiveViewModelInstance*, const char* path);
RiveStatus         rive_vmi_list_size(RiveViewModelInstance*, const char* path, uint32_t* out);
RiveViewModelInstance* rive_vmi_list_instance_at(RiveViewModelInstance*, const char* path,
                                                 uint32_t index);
uint32_t           rive_vmi_property_count(RiveViewModelInstance*);
RiveStatus         rive_vmi_property_at(RiveViewModelInstance*, uint32_t index,
                                        char* name_buf, size_t cap,
                                        size_t* out_len, int* out_type);
RiveStatus         rive_vmi_get_number(RiveViewModelInstance*, const char* path, float* out);
RiveStatus         rive_vmi_get_bool(RiveViewModelInstance*, const char* path, uint8_t* out);
RiveStatus         rive_vmi_get_color(RiveViewModelInstance*, const char* path, uint32_t* out);
RiveStatus         rive_vmi_get_string(RiveViewModelInstance*, const char* path,
                                       char* buf, size_t cap, size_t* out_len);
RiveStatus         rive_vmi_get_enum_index(RiveViewModelInstance*, const char* path, uint32_t* out);
RiveStatus         rive_vmi_flush_changed(RiveViewModelInstance*, const char* path, uint8_t* out);
/* Handle writes — drive a nested VM or a LIST ITEM (the flat path can't index lists). */
RiveStatus         rive_vmi_set_number(RiveViewModelInstance*, const char* path, float value);
RiveStatus         rive_vmi_set_bool(RiveViewModelInstance*, const char* path, uint8_t value);
RiveStatus         rive_vmi_set_color(RiveViewModelInstance*, const char* path, uint32_t argb);
RiveStatus         rive_vmi_set_string(RiveViewModelInstance*, const char* path, const char* value);
RiveStatus         rive_vmi_set_enum_index(RiveViewModelInstance*, const char* path, uint32_t index);
RiveStatus         rive_vmi_set_enum_name(RiveViewModelInstance*, const char* path, const char* name);
RiveStatus         rive_vmi_fire_trigger(RiveViewModelInstance*, const char* path);
/* Bind a decoded image to a nested-VM or LIST-ITEM image property; NULL clears. */
RiveStatus         rive_vmi_set_image(RiveViewModelInstance*, const char* path, RiveImage* image);
/* Bind a file-sourced BindableArtboard to a nested-VM or LIST-ITEM artboard property; NULL clears. */
RiveStatus         rive_vmi_set_artboard(RiveViewModelInstance*, const char* path,
                                         RiveBindableArtboard* bindable);

/* --- Text runs (get/set a TextValueRun's string) ----------------------------
 * Read / write a named text run's string at runtime. `name` is the run's
 * authored name; `path` selects a NESTED artboard ('/'-style, per
 * ArtboardInstance::getTextRun), or is NULL/"" for the top-level artboard.
 * Setting re-shapes the run on the next advance/draw. `_get`/`_name_at` use the
 * two-call buffer protocol (buf=NULL, cap=0 to size first; bytes are NOT
 * NUL-terminated). Return nonzero (+ rive_last_error) on a missing run / bad
 * handle. Introspection (`_count`/`_name_at`) lists TOP-LEVEL runs only.
 * Implemented in rive_shim_text.cpp. */
RiveStatus         rive_artboard_text_set(RiveArtboard*, const char* name,
                                          const char* path, const char* value);
RiveStatus         rive_artboard_text_get(RiveArtboard*, const char* name,
                                          const char* path, char* buf, size_t cap,
                                          size_t* out_len);
uint32_t           rive_artboard_text_run_count(RiveArtboard*);
RiveStatus         rive_artboard_text_run_name_at(RiveArtboard*, uint32_t index,
                                                  char* buf, size_t cap, size_t* out_len);

/* --- Rig runtime control (bones / constraints / solo) -----------------------
 * Drive a rig at runtime by AUTHORED component name (ArtboardInstance::find<T>).
 * Like text / VM writes, a set is asserted on the artboard and takes effect on
 * the next advance/draw (advance solves on top — a written value sticks only if
 * the active animation does not ALSO key that property). Introspection
 * (`rig_count`/`rig_name_at`) lists components of a kind so a game can discover
 * the settable names in an opaque .riv. Implemented in rive_shim_rig.cpp. */

/* Bone property selector for rive_artboard_bone_get/_set. ROTATION/SCALE_X/
 * SCALE_Y/LENGTH apply to ANY bone (find<Bone>); X/Y apply to ROOT bones only
 * (find<RootBone>) — a get/set of X/Y on a non-root bone is an error. */
#define RIVE_BONE_ROTATION 0
#define RIVE_BONE_SCALE_X  1
#define RIVE_BONE_SCALE_Y  2
#define RIVE_BONE_LENGTH   3
#define RIVE_BONE_X        4
#define RIVE_BONE_Y        5
RiveStatus         rive_artboard_bone_set(RiveArtboard*, const char* name,
                                          uint32_t prop, float value);
RiveStatus         rive_artboard_bone_get(RiveArtboard*, const char* name,
                                          uint32_t prop, float* out);

/* Constraint strength (typically [0,1]) — every Constraint has it. */
RiveStatus         rive_artboard_constraint_set_strength(RiveArtboard*, const char* name,
                                                         float value);
RiveStatus         rive_artboard_constraint_get_strength(RiveArtboard*, const char* name,
                                                         float* out);

/* Solo: exclusive visibility among children. Set the active child by name or
 * 0-based index (nonzero + rive_last_error if that child / index doesn't exist);
 * read the active child's name (two-call) / index (-1 if none active). */
RiveStatus         rive_artboard_solo_set_active_name(RiveArtboard*, const char* name,
                                                      const char* child);
RiveStatus         rive_artboard_solo_set_active_index(RiveArtboard*, const char* name,
                                                       uint32_t index);
RiveStatus         rive_artboard_solo_get_active_name(RiveArtboard*, const char* name,
                                                      char* buf, size_t cap, size_t* out_len);
int32_t            rive_artboard_solo_get_active_index(RiveArtboard*, const char* name);

/* Generalized introspection: list rig components of `kind` by authored name (in
 * artboard object order; two-call protocol on `_name_at`). Bone count includes
 * root bones; constraint count includes every concrete constraint type. */
#define RIVE_RIG_BONE       0
#define RIVE_RIG_CONSTRAINT 1
#define RIVE_RIG_SOLO       2
uint32_t           rive_artboard_rig_count(RiveArtboard*, uint32_t kind);
RiveStatus         rive_artboard_rig_name_at(RiveArtboard*, uint32_t kind, uint32_t index,
                                             char* buf, size_t cap, size_t* out_len);

/* --- Runtime input (joystick / keyboard / gamepad / focus) ------------------
 * Two shapes of host-driven input (see rive_shim_input.cpp):
 *  - JOYSTICK is an AUTHORED component (like a bone), set by name on a RiveArtboard
 *    and APPLIED during advance (drives linked animations/constraints) — a set
 *    sticks unless an animation also keys it.
 *  - KEYBOARD / GAMEPAD / FOCUS are a state-machine EVENT feed on a RiveStateMachine,
 *    routed via the SM's FocusManager (focus tree auto-built at SM creation) to the
 *    focused FocusData's listeners. They no-op (return 0 / consumed=false) on a
 *    handle whose scene is the animation fallback, or when the .riv authors no
 *    FocusData. */

/* Joystick: normalized x/y in [-1,1]. `_count`/`_name_at` (two-call protocol) list
 * the authored joysticks so a game can discover the settable names. */
RiveStatus         rive_artboard_joystick_set(RiveArtboard*, const char* name, float x, float y);
RiveStatus         rive_artboard_joystick_get(RiveArtboard*, const char* name,
                                              float* out_x, float* out_y);
uint32_t           rive_artboard_joystick_count(RiveArtboard*);
RiveStatus         rive_artboard_joystick_name_at(RiveArtboard*, uint32_t index,
                                                  char* buf, size_t cap, size_t* out_len);

/* Keyboard: `key` is a rive::Key code (GLFW layout, e.g. 'A'=65, space=32, arrows
 * 262-265); `modifiers` is a rive::KeyModifiers bitmask (shift=1/ctrl=2/alt=4/meta=8).
 * `text` is committed/IME UTF-8. Both return 1 if a listener consumed the event. */
uint8_t            rive_state_machine_key_input(RiveStateMachine*, uint16_t key,
                                                uint8_t modifiers, uint8_t is_pressed,
                                                uint8_t is_repeat);
uint8_t            rive_state_machine_text_input(RiveStateMachine*, const char* utf8, size_t len);

/* Gamepad: `button`/`axis` are W3C Standard Gamepad indices (button: south=0,east=1,
 * …,start=16; axis: leftX=0,leftY=1,rightX=2,rightY=3,leftTrigger=4,rightTrigger=5).
 * `value` folds into a cumulative per-SM snapshot then dispatches; a button reads as
 * pressed at `value >= 0.5` (rive's listener threshold — pass 1.0 to press, 0.0 to
 * release). Return 1 if consumed by the focus tree. */
uint8_t            rive_state_machine_gamepad_button(RiveStateMachine*, uint8_t button,
                                                     float value);
uint8_t            rive_state_machine_gamepad_axis(RiveStateMachine*, uint8_t axis, float value);

/* Focus: advance the primary focus in a direction (tab order or spatial), clear it,
 * or poll the state (for showing a soft keyboard). `_advance` returns 1 if focus
 * moved. */
#define RIVE_FOCUS_NEXT  0
#define RIVE_FOCUS_PREV  1
#define RIVE_FOCUS_LEFT  2
#define RIVE_FOCUS_RIGHT 3
#define RIVE_FOCUS_UP    4
#define RIVE_FOCUS_DOWN  5
uint8_t            rive_state_machine_focus_advance(RiveStateMachine*, uint32_t dir);
void               rive_state_machine_clear_focus(RiveStateMachine*);
void               rive_state_machine_focus_state(RiveStateMachine*, uint8_t* out_has_focus,
                                                  uint8_t* out_expects_keyboard);

/* --- Audio (engine lifecycle + master volume) -------------------------------
 * With --with_rive_audio=system, rive plays audio events / embedded audio to the
 * OS output automatically during advance (the lazily-created singleton
 * AudioEngine::RuntimeEngine). These expose the host BRIDGE controls. Built
 * without audio, they report unavailable / no-op (stable ABI). Implemented in
 * rive_shim_audio.cpp. */
uint8_t            rive_audio_is_available(void); /* 1 if audio compiled in */
uint8_t            rive_audio_start(void);        /* open/resume device; 1 if engine present */
void               rive_audio_stop(void);         /* pause + release device (no-op if none) */
void               rive_audio_set_volume(float volume); /* 0 = mute, 1 = unity (both modes) */

/* External (host-mixer) mode — --with_rive_audio=external (EXTERNAL_RIVE_AUDIO_ENGINE,
 * the `audio-external` cargo feature). rive owns NO device; the host PULLS the mixed
 * interleaved f32 PCM and routes it to its own mixer. The engine clock advances only
 * as the host reads. Built in any other mode these are inert stubs (report 0 / write
 * nothing) for a stable ABI. `frames` holds num_frames * channels() floats. */
uint32_t           rive_audio_channels(void);     /* PCM channels (default 2); 0 if N/A */
uint32_t           rive_audio_sample_rate(void);  /* PCM sample rate Hz (default 48000) */
uint64_t           rive_audio_read_frames(float* frames, uint64_t num_frames); /* -> frames written */
uint8_t            rive_audio_sum_frames(float* frames, uint64_t num_frames);  /* mix-add; 1 = ok */

/* --- Frame: begin -> draw -> flush ----------------------------------------- */

/* Begins a frame against `target`, clearing to the given straight (non-
 * premultiplied) RGBA color in [0, 1]. Exactly one frame may be in flight per
 * context. */
RiveStatus         rive_frame_begin(RiveRenderContext* ctx,
                                    RiveRenderTarget* target,
                                    float r, float g, float b, float a);

/* Draws `artboard` into the current frame, fit with Fit::contain +
 * Alignment::center to the target. Call after advancing. */
RiveStatus         rive_artboard_draw(RiveArtboard* artboard,
                                      RiveRenderContext* ctx);

/* Like rive_artboard_draw, but fits the artboard (Fit::contain + center) into the
 * sub-rect (x,y,w,h) of the bound target — an ATLAS TILE, in target pixels — and
 * CLIPS to that rect so content cannot bleed into neighboring tiles. The clip uses
 * rive's cheap axis-aligned clipRect shader path (no mask draw). Call between a
 * begin and a record/flush, like rive_artboard_draw. */
RiveStatus         rive_artboard_draw_viewport(RiveArtboard* artboard,
                                               RiveRenderContext* ctx,
                                               float x, float y,
                                               float w, float h);

/* Submits the frame, copies the result back to a CPU buffer held by the target,
 * and waits for the GPU. After this, use rive_render_target_read_pixels. */
RiveStatus         rive_frame_flush(RiveRenderContext* ctx);

/* --- Readback (M0 validation) ---------------------------------------------- */

/* Copies the most recently flushed frame's pixels into `out_rgba`.
 * `out_len` must equal rive_render_target_pixel_buffer_size(). Pixels are
 * RGBA8, top-down, sRGB-encoded, with PREMULTIPLIED alpha. */
RiveStatus         rive_render_target_read_pixels(RiveRenderTarget* target,
                                                  uint8_t* out_rgba,
                                                  size_t out_len);

/* ====================================================================== *
 * M1b: external (wgpu-shared) Vulkan tier — ZERO-COPY shared VkImage.
 *
 * In M1b, wgpu owns the VkInstance/VkPhysicalDevice/VkDevice/VkQueue; the shim
 * BORROWS them (never creates/destroys them) and renders the .riv directly into
 * a wgpu-allocated VkImage. rive's flush RECORDS into a command buffer the shim
 * allocates from its own per-frame pool; the shim then submits OUT-OF-BAND to
 * the wgpu graphics queue with a caller-owned VkFence. rive itself never
 * submits. The caller (Rust/Bevy) owns the pool family, the queue, and the
 * fence lifecycle, and waits the fence before the sampling pass.
 *
 * All Vulkan handles cross this ABI as `uint64_t` (the integer value of the
 * dispatchable/non-dispatchable handle, as exposed by wgpu-hal/ash), so the
 * ABI itself carries no Vulkan headers. 64-bit hosts only (dispatchable handles
 * are pointers).
 * ====================================================================== */

/* rive's PLS interlock mode (gpu::InterlockMode ordinals; pinned by
 * static_assert in the .cpp). -1 == null handle / not currently in a frame. */
typedef int32_t RivePlsMode;
#define RIVE_PLS_RASTER_ORDERING  0
#define RIVE_PLS_ATOMICS          1
#define RIVE_PLS_CLOCKWISE        2
#define RIVE_PLS_CLOCKWISE_ATOMIC 3
#define RIVE_PLS_MSAA             4

/* Mirror of rive::gpu::VulkanFeatures (vulkan_context.hpp). The caller fills
 * this from what wgpu ACTUALLY enabled on the shared VkDevice. C-stable layout;
 * the shim copies field-by-field into rive's struct (never reinterpret-casts).
 * Bools are int32 (0/nonzero) for a stable ABI. */
typedef struct RiveVulkanFeatures {
    uint32_t apiVersion;                              /* e.g. VK_API_VERSION_1_1 (0x00401000) */
    int32_t  independentBlend;
    int32_t  fillModeNonSolid;
    int32_t  fragmentStoresAndAtomics;               /* REQUIRED for core operation (atomic fallback) */
    int32_t  shaderClipDistance;
    int32_t  rasterizationOrderColorAttachmentAccess;/* EXT_rasterization_order_attachment_access */
    int32_t  fragmentShaderPixelInterlock;           /* VK_EXT_fragment_shader_interlock */
    int32_t  vkKhrPortabilitySubset;
    int32_t  textureCompressionBC;
    int32_t  textureCompressionASTC_LDR;
    int32_t  textureCompressionETC2;
} RiveVulkanFeatures;

/* Create a rive RenderContext on a wgpu-OWNED Vulkan device. The shim does NOT
 * create or destroy the instance/device — it only borrows them.
 *
 *   instance/physicalDevice/device : the wgpu-owned VkInstance/VkPhysicalDevice/VkDevice
 *   getInstanceProcAddr            : PFN_vkGetInstanceProcAddr (a raw fn pointer value)
 *   features                       : MUST mirror exactly what wgpu enabled on `device`
 *   forceAtomic                    : if nonzero, ContextOptions.forceAtomicMode = true
 *
 * Returns NULL on failure. Destroy with rive_render_context_destroy (which, for
 * an external context, resets only the RenderContext and never touches the
 * device/instance). */
RiveRenderContext* rive_render_context_create_vulkan_external(
    uint64_t instance,
    uint64_t physicalDevice,
    uint64_t device,
    void*    getInstanceProcAddr,            /* PFN_vkGetInstanceProcAddr */
    const RiveVulkanFeatures* features,
    int32_t  forceAtomic);

/* The graphics queue-family index the shim allocates its per-frame command pool
 * on. Call ONCE after creating an external context, before the first frame.
 * (Stored on the context; the pool is created lazily on first submit.) */
void rive_render_context_set_queue_family(RiveRenderContext* ctx,
                                          uint32_t queueFamilyIndex);

/* M2.0 perf lever: per-frame `clockwiseFillOverride` (rive FrameDescriptor). When
 * nonzero, rive's select_interlock_mode prefers its clockwise PLS path (clockwise
 * if the device supports it, else clockwiseAtomic) over atomics — relevant on
 * desktop NVIDIA, which lacks the raster-order ext so its default path is atomics.
 * Off by default; set once after create. Honored by rive_frame_begin_external. */
void rive_render_context_set_clockwise(RiveRenderContext* ctx, int32_t enabled);

/* Frame-independent: does the shared VkDevice give rive its clean raster-order
 * PLS path? 1 == yes, 0 == no (atomic/msaa fallback), -1 == null handle. Valid
 * any time after create. */
int32_t rive_render_context_supports_raster_ordering(const RiveRenderContext* ctx);

/* Active per-frame interlock mode (gpu::InterlockMode ordinal; see RIVE_PLS_*).
 * Valid ONLY between rive_frame_begin_external and rive_frame_submit_external.
 * -1 on null. */
RivePlsMode rive_render_context_pls_mode(const RiveRenderContext* ctx);

/* M2.0: GPU execution time (milliseconds) of the most recent external frame's
 * rive command buffer, measured with VkQueryPool timestamps written around rive's
 * recorded work (begin -> flush -> post-flush barrier). The blocking submit
 * guarantees the result is ready on return. Returns -1.0 if GPU timing is
 * unavailable (device lacks reliable timestamps, or the timestamp PFNs/pool could
 * not be set up). */
double rive_render_context_last_gpu_ms(const RiveRenderContext* ctx);

/* M2a: CPU-side sub-span timings of the most recent external frame, microseconds,
 * for the fence-vs-flush perf split (Step 0). `flush_us` is rive's CPU-side
 * RenderContext::flush() (command-buffer record + rive's own CPU work); the
 * `fence_wait_us` is the blocking vkWaitForFences after the out-of-band submit
 * (the cost the M2a non-blocking-sync rework targets). -1.0 if no external frame
 * has run yet. The remainder of render_external_frame's wall (begin/end CB, the
 * post-flush barrier, ResetFences, QueueSubmit, timestamp readback) is "other" =
 * total - flush - fence_wait. */
double rive_render_context_last_flush_us(const RiveRenderContext* ctx);
double rive_render_context_last_fence_wait_us(const RiveRenderContext* ctx);

/* Wrap a wgpu-ALLOCATED VkImage as a rive render target (ZERO COPY). The shim
 * does NOT allocate or free the image — wgpu owns it. If `vkImageView` is 0 the
 * shim creates a matching view (via makeExternalImageView) and owns THAT view
 * only. `vkFormat` is the wgpu texture's VkFormat (Rgba8Unorm == 37 ==
 * VK_FORMAT_R8G8B8A8_UNORM); `vkUsageFlags` is the VkImageUsageFlags wgpu
 * created it with (must include INPUT_ATTACHMENT or both TRANSFER_SRC+DST per
 * rive's render-target contract — the Rust side allocates
 * RENDER_ATTACHMENT|TEXTURE_BINDING|COPY_DST|COPY_SRC).
 *
 * Returns NULL on failure. Destroy with rive_render_target_destroy (which, for
 * an external target, drops the rive wrapper + any shim-created view, and never
 * frees the wgpu image). */
RiveRenderTarget* rive_render_target_wrap_vk_image(
    RiveRenderContext* ctx,
    uint64_t vkImage,
    uint64_t vkImageView,
    uint32_t width,
    uint32_t height,
    uint32_t vkFormat,
    uint32_t vkUsageFlags);

/* Rebind the wgpu VkImage/view on an existing external target (e.g. after the
 * GpuImage was reprepared/resized). Pass vkImageView=0 to have the shim
 * recreate the view. Resets the tracked layout to UNDEFINED. */
void rive_render_target_set_vk_image(RiveRenderTarget* target,
                                     uint64_t vkImage,
                                     uint64_t vkImageView);

/* Begin a frame against a wrapped external target. Like rive_frame_begin but
 * with no synchronizer; the caller supplies the frame-number watermark:
 *   currentFrameNumber : monotonically increasing, MUST be nonzero
 *   safeFrameNumber    : highest frame the caller has OBSERVED the GPU finished
 * Clear color is straight (non-premultiplied) RGBA in [0,1]. */
RiveStatus rive_frame_begin_external(RiveRenderContext* ctx,
                                     RiveRenderTarget* target,
                                     float r, float g, float b, float a,
                                     uint64_t currentFrameNumber,
                                     uint64_t safeFrameNumber);

/* (Draw with rive_artboard_draw — it is REUSED verbatim for both tiers.) */

/* Record rive's draws + the post-flush COLOR->SHADER_READ_ONLY barrier into a
 * command buffer the shim allocates from its per-frame pool (on the queue family
 * set above), then vkEndCommandBuffer + vkQueueSubmit OUT-OF-BAND to `queue`
 * with a shim-internal fence, then BLOCK on that fence. rive RECORDS; the shim
 * owns begin/end/submit/wait. NO readback, NO pixel flip. On return the shared
 * image is fully written and left in SHADER_READ_ONLY_OPTIMAL, ready to sample.
 *
 *   queue : the wgpu graphics VkQueue (the Rust side serializes against wgpu's
 *           queue use; see the M1b report)
 *
 * The fence is internal (the Rust side cannot cheaply build an ash::Device to
 * make one). M1b is correctness-first: this call is BLOCKING. Splitting submit
 * from wait for pipelining is M2. */
RiveStatus rive_frame_submit_external(RiveRenderContext* ctx,
                                      RiveRenderTarget* target,
                                      uint64_t queue);

/* M2a NON-BLOCKING path. Like rive_frame_submit_external, but RECORDS rive's draws
 * + the ->SHADER_READ_ONLY barrier into `cmdBuffer` — the CALLER's already-open
 * command buffer (wgpu's own, from as_hal_mut().raw_handle()) — and returns WITHOUT
 * begin/end/submit/fence. rive's work rides wgpu's single per-frame submit,
 * GPU-ordered before the wgpu pass that samples the image; no CPU stall.
 *
 *   cmdBuffer : wgpu's open primary VkCommandBuffer for this frame (u64 handle).
 *
 * The caller must seed safeFrameNumber (at begin) to trail currentFrameNumber by
 * rive's ring size (no fence → a frame is recyclable only once its GPU work has
 * completed, bounded by frames-in-flight). On return the image is left in
 * SHADER_READ_ONLY_OPTIMAL == wgpu's tracked RESOURCE layout. */
RiveStatus rive_frame_record_external(RiveRenderContext* ctx,
                                      RiveRenderTarget* target,
                                      uint64_t cmdBuffer);

/* The VkImage / VkImageView the external target currently points at (0 if not
 * external). Diagnostics. */
uint64_t rive_render_target_vk_image(const RiveRenderTarget* target);
uint64_t rive_render_target_vk_image_view(const RiveRenderTarget* target);

/* ---- Backend-tagged d3d12 / metal siblings (DESIGN ONLY; stubbed in M1b) ----
 *
 * Declared so the cross-backend ABI shape is uniform and M2/M3 can implement
 * them without ABI churn. In this build they set rive_last_error and return
 * NULL / nonzero. The signatures encode each backend's submission model:
 *   - Vulkan : VkCommandBuffer recorded by rive, submitted by us to a VkQueue
 *              with a VkFence (above).
 *   - D3D12  : rive records into its own command list; the caller drives an
 *              ID3D12CommandQueue + ID3D12Fence/value (no external cmd buffer).
 *   - Metal  : rive's FlushResources.externalCommandBuffer is id<MTLCommandBuffer>,
 *              which self-submits via `commit`.
 */
RiveRenderContext* rive_render_context_create_d3d12_external(
    void* d3d12Device, void* d3d12CommandQueue, int32_t forceAtomic);
RiveRenderTarget*  rive_render_target_wrap_d3d12_resource(
    RiveRenderContext* ctx, void* d3d12Resource,
    uint32_t width, uint32_t height, uint32_t dxgiFormat);
RiveStatus         rive_frame_submit_external_d3d12(
    RiveRenderContext* ctx, RiveRenderTarget* target,
    void* d3d12CommandQueue, void* d3d12Fence, uint64_t fenceValue);

RiveRenderContext* rive_render_context_create_metal_external(
    void* mtlDevice, void* mtlCommandQueue);
RiveRenderTarget*  rive_render_target_wrap_metal_texture(
    RiveRenderContext* ctx, void* mtlTexture,
    uint32_t width, uint32_t height, uint32_t mtlPixelFormat);
RiveStatus         rive_frame_submit_external_metal(
    RiveRenderContext* ctx, RiveRenderTarget* target,
    void* mtlCommandBuffer /* caller-owned id<MTLCommandBuffer> */);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* RIVE_SHIM_H */
