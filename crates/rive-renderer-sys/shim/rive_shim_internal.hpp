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
#include "rive/layout.hpp"                        // Fit, Alignment
#include "rive/refcnt.hpp"                        // rcp, make_rcp
#include "rive/viewmodel/viewmodel_instance.hpp" // ViewModelInstance
#include "rive/viewmodel/runtime/viewmodel_instance_runtime.hpp" // ViewModelInstanceRuntime

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

// Cross-TU error reporter. The canonical setter has internal linkage in
// rive_shim.cpp; feature TUs call this to populate rive_last_error().
void shim_set_error(const char* msg);

#endif // RIVE_SHIM_INTERNAL_HPP
