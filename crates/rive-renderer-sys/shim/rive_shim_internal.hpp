/*
 * rive_shim_internal.hpp — definitions shared ACROSS the shim's translation
 * units (rive_shim.cpp + the per-feature TUs, e.g. rive_shim_viewmodel.cpp).
 *
 * The flat C ABI + the opaque handle typedefs are in rive_shim.h (public, what
 * Rust binds). This header DEFINES the opaque handle structs that more than one
 * shim TU touches, plus small cross-TU helpers. It is NOT part of the public
 * ABI and is never seen by Rust/bindgen.
 *
 * Feature-wiring convention (see docs/feature-support.md): each Rive feature
 * area gets its own rive_shim_<feature>.cpp; it #includes this header to reach
 * the handle structs + helpers, so feature code stays in its own navigable TU
 * rather than growing rive_shim.cpp. Only structs a second TU needs live here;
 * the rest stay file-local in rive_shim.cpp until a feature pulls them out.
 */
#ifndef RIVE_SHIM_INTERNAL_HPP
#define RIVE_SHIM_INTERNAL_HPP

#include "rive_shim.h" // RiveStatus, RIVE_OK, opaque handle typedefs

#include <memory>

#include "rive/artboard.hpp"                     // ArtboardInstance
#include "rive/scene.hpp"                         // Scene (RiveStateMachine.scene)
#include "rive/layout.hpp"                        // Fit, Alignment
#include "rive/refcnt.hpp"                        // rcp, make_rcp
#include "rive/input/gamepad_snapshot.hpp"        // GamepadSnapshot (RiveStateMachine cumulative pad state)
#include "rive/viewmodel/viewmodel_instance.hpp" // ViewModelInstance
#include "rive/viewmodel/runtime/viewmodel_instance_runtime.hpp" // ViewModelInstanceRuntime

namespace rive {
class RenderImage; // full type only needed where a RiveImage is built/destroyed
// Typed alias of RiveStateMachine.scene used by the input TU (keyboard/gamepad/
// focus live on StateMachineInstance, not the base Scene). The runtime is built
// -fno-rtti, so we can't downcast `scene` back — instead the selectors capture
// the concrete pointer at construction (null for a LinearAnimationInstance scene).
class StateMachineInstance;
}

// An OWNED, decoded render image — the value source for image-property data
// binding. Created by rive_image_decode (which needs a RiveRenderContext, since
// decoding goes through the render context's rive::Factory) and released by
// rive_image_destroy. DEFINED here (not in rive_shim.cpp) so the view-model TU can
// read `image` to feed propertyImage()->value(). The image setter takes its OWN
// ref on bind, so a RiveImage may be destroyed after binding without unbinding it.
struct RiveImage
{
    rive::rcp<rive::RenderImage> image;
};

// One artboard instance + its bound default view model. DEFINED here (not in
// rive_shim.cpp) so the view-model TU can reach `vmRuntime`. The other opaque
// handle structs stay in rive_shim.cpp until a second TU needs them.
struct RiveArtboard
{
    std::unique_ptr<rive::ArtboardInstance> artboard;
    // The artboard's default view-model instance, bound so editor-authored data
    // bindings (incl. scripted view-model inputs) resolve at runtime. Held here
    // so the SAME instance is also bound to the state machine. Null for artboards
    // with no view model.
    rive::rcp<rive::ViewModelInstance> vmInstance;
    // Runtime wrapper over the SAME `vmInstance` for name-based property get/set
    // (data binding — see the Rive data-binding docs (https://rive.app/docs)). Wraps the already-bound
    // instance; it does NOT create a new one, so it never disturbs the
    // script/data-binding context. Null whenever `vmInstance` is.
    rive::rcp<rive::ViewModelInstanceRuntime> vmRuntime;
    // How the artboard is aligned into the draw target (rive_artboard_draw /
    // _viewport read these). Default contain/center/1.0 == the historical
    // hardcoded behavior, so unset artboards render byte-identically. Set via
    // rive_artboard_set_fit_align (the RiveFit component).
    rive::Fit fit = rive::Fit::contain;
    rive::Alignment alignment = rive::Alignment::center;
    float scaleFactor = 1.0f; // only used by Fit::layout
};

// One playable scene (state machine / animation) + the pointer & input state that
// rides on it. DEFINED here (not in rive_shim.cpp) so the input TU
// (rive_shim_input.cpp) can reach `smInstance` for keyboard / gamepad / focus,
// which live on StateMachineInstance — the base `scene` virtuals (pointer*,
// advanceAndApply) cover the rest. The pointer/fit/seek functions stay in
// rive_shim.cpp; only the struct is shared.
struct RiveStateMachine
{
    std::unique_ptr<rive::Scene> scene;
    // Concrete-type capture (the runtime is built -fno-rtti, so we can't downcast).
    // `smInstance` aliases `scene` when it is a StateMachineInstance (the keyboard /
    // gamepad / focus entry points need it); null for the LinearAnimationInstance
    // fallback. `isLinear` is the complementary flag the seek/time playhead API casts
    // on (true ONLY for that animation fallback). Both set definitively by the
    // selectors, NOT inferred from durationSeconds() (a StaticScene would alias 0).
    rive::StateMachineInstance* smInstance = nullptr;
    bool isLinear = false;
    // Cumulative W3C-standard gamepad state (deviceId 0). rive_state_machine_gamepad_*
    // mutate this then dispatch a GamepadEventInvocation built from it, so a held
    // button / settled axis stays reflected in buttonMask / axes across calls (a
    // script reading fullState sees the real state, not just the last delta).
    rive::GamepadSnapshot gamepad;
    // Fit/alignment for INVERTING pointer coords back into artboard space — must
    // mirror the artboard's draw fit/alignment or hits won't line up. Default
    // contain/center == the historical hardcoded inversion. Set via
    // rive_state_machine_set_fit_align (kept in sync with the artboard's by the
    // RiveFit component).
    rive::Fit fit = rive::Fit::contain;
    rive::Alignment alignment = rive::Alignment::center;
    float scaleFactor = 1.0f;
    // Atlas pointer mapping: the DRAWN tile size (px) an atlas face renders into via
    // rive_artboard_draw_viewport. When both are > 0, pointer coords (in the face's
    // logical target space) are normalized into this tile before the fit/alignment is
    // inverted — because an atlas face is fit into its tile, not the full target. 0
    // (the default) = full-target inversion, i.e. the historical dedicated-face path.
    // Set per-frame by the atlas node via rive_state_machine_set_pointer_tile.
    float ptrTileW = 0.0f;
    float ptrTileH = 0.0f;
};

// Cross-TU error reporter. The canonical setter has internal linkage in
// rive_shim.cpp; feature TUs call this to populate rive_last_error().
void shim_set_error(const char* msg);

#endif // RIVE_SHIM_INTERNAL_HPP
