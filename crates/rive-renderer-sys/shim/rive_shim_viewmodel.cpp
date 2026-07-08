/*
 * rive_shim_viewmodel.cpp — view-model data binding C ABI.
 *
 * Per-feature shim TU (see rive_shim_internal.hpp + docs/feature-support.md):
 * get/set named view-model properties on an artboard's bound DEFAULT view-model
 * instance, via the ViewModelInstanceRuntime wrapper held on RiveArtboard. The
 * API mirrors the Rive data-binding docs (https://rive.app/docs).
 *
 * Two surfaces share one core:
 *   - artboard-rooted (`rive_artboard_vm_*`): flat path on the artboard's root
 *     view model; `/` reaches into named nested view models. number/bool/trigger/
 *     color/string/enum get+set + top-level schema introspection.
 *   - handle-based (`rive_vmi_*`): an opaque RiveViewModelInstance* handle for a
 *     view-model INSTANCE (the root, a nested VM, or a list item). Adds nested-VM
 *     introspection (recurse `propertyViewModel`), list size + item access
 *     (`propertyList`/`instanceAt` — the native path resolver can't index lists),
 *     and reads + writes. Handles are BORROWED: they alias instances owned by rive's
 *     caches under the root `vmRuntime`, so they are valid only while the artboard
 *     lives and the addressed list is not mutated. Reads AND writes (number/bool/
 *     color/string/enum/trigger + image + artboard) — so a caller can drive a nested VM or a LIST ITEM.
 *     An image (assetImage) property is bound from a decoded RiveImage (rive_image_decode
 *     lives in rive_shim.cpp); an artboard property is bound from a RiveBindableArtboard
 *     (rive_file_bindable_artboard_* — also in rive_shim.cpp).
 *
 * A third block (at the bottom) adds INSTANCE CONSTRUCTION + LIST STRUCTURAL MUTATION:
 * a RiveViewModelRuntime* is a view-model DEFINITION (reached from an artboard's stashed
 * File*) that mints fresh, caller-OWNED RiveOwnedVmInstance instances; those are populated
 * via the borrow-into-the-get/set-surface trick (rive_owned_vmi_borrow), then added to a
 * list (rive_vmi_list_add*) / assigned to a VM-ref property (rive_vmi_replace_view_model),
 * after which the list/parent co-owns them.
 */
#include "rive_shim_internal.hpp"

#include <cstring>
#include <vector>

#include "rive/viewmodel/runtime/viewmodel_instance_number_runtime.hpp"
#include "rive/viewmodel/runtime/viewmodel_instance_boolean_runtime.hpp"
#include "rive/viewmodel/runtime/viewmodel_instance_trigger_runtime.hpp"
#include "rive/viewmodel/runtime/viewmodel_instance_color_runtime.hpp"
#include "rive/viewmodel/runtime/viewmodel_instance_string_runtime.hpp"
#include "rive/viewmodel/runtime/viewmodel_instance_enum_runtime.hpp"
#include "rive/viewmodel/runtime/viewmodel_instance_list_runtime.hpp"
#include "rive/viewmodel/runtime/viewmodel_instance_asset_image_runtime.hpp" // image binding
#include "rive/viewmodel/runtime/viewmodel_instance_artboard_runtime.hpp" // artboard-ref binding
#include "rive/viewmodel/runtime/viewmodel_runtime.hpp" // PropertyData + createInstance*
#include "rive/bindable_artboard.hpp" // BindableArtboard (RiveBindableArtboard::bindable)
#include "rive/file.hpp" // File::viewModelBy* (VM-runtime construction from an artboard)

using rive::ViewModelInstanceRuntime;

// Copies `s` into a caller buffer per the two-call protocol: always sets
// *out_len to the full length (call with cap=0 to size), copies min(cap, len)
// bytes (no NUL terminator — the caller slices to *out_len).
namespace {
void copy_to_caller(const std::string& s, char* buf, size_t cap, size_t* out_len)
{
    if (out_len != nullptr)
        *out_len = s.size();
    if (buf != nullptr && cap > 0)
    {
        const size_t n = s.size() < cap ? s.size() : cap;
        std::memcpy(buf, s.data(), n);
    }
}

// ---- handle <-> runtime casts (the opaque RiveViewModelInstance is a
// rive::ViewModelInstanceRuntime under the hood) ----
inline ViewModelInstanceRuntime* as_vmi(RiveViewModelInstance* h)
{
    return reinterpret_cast<ViewModelInstanceRuntime*>(h);
}
inline RiveViewModelInstance* as_handle(ViewModelInstanceRuntime* p)
{
    return reinterpret_cast<RiveViewModelInstance*>(p);
}

// The artboard's bound root view-model runtime, or null (+ error) if it has none.
ViewModelInstanceRuntime* root_vm(RiveArtboard* artboard)
{
    if (artboard == nullptr || artboard->vmRuntime == nullptr)
    {
        shim_set_error("artboard has no view model");
        return nullptr;
    }
    return artboard->vmRuntime.get();
}

// Root VM + a non-null path guard (mirrors the original combined check order:
// no-view-model first, then null-path), for the artboard-rooted ABI wrappers.
ViewModelInstanceRuntime* root_vm_path(RiveArtboard* artboard, const char* path)
{
    ViewModelInstanceRuntime* vm = root_vm(artboard);
    if (vm == nullptr)
        return nullptr;
    if (path == nullptr)
    {
        shim_set_error("view-model property path is null");
        return nullptr;
    }
    return vm;
}

// A handle + a non-null path guard, for the handle-based ABI.
ViewModelInstanceRuntime* vmi_path(RiveViewModelInstance* handle, const char* path)
{
    if (handle == nullptr)
    {
        shim_set_error("view-model instance handle is null");
        return nullptr;
    }
    if (path == nullptr)
    {
        shim_set_error("view-model property path is null");
        return nullptr;
    }
    return as_vmi(handle);
}

// ===== vmi-core: the real get/set logic on a resolved (non-null) runtime + a
// non-null path. The two ABI surfaces (artboard-rooted + handle) both delegate
// here, so there is one source of truth per property type. =====

RiveStatus vmi_set_number(ViewModelInstanceRuntime* vm, const char* path, float value)
{
    auto* p = vm->propertyNumber(path);
    if (p == nullptr)
    {
        shim_set_error("view-model number property not found");
        return 1;
    }
    p->value(value);
    return RIVE_OK;
}
RiveStatus vmi_get_number(ViewModelInstanceRuntime* vm, const char* path, float* out)
{
    if (out == nullptr)
    {
        shim_set_error("out pointer is null");
        return 1;
    }
    auto* p = vm->propertyNumber(path);
    if (p == nullptr)
    {
        shim_set_error("view-model number property not found");
        return 1;
    }
    *out = p->value();
    return RIVE_OK;
}
RiveStatus vmi_set_bool(ViewModelInstanceRuntime* vm, const char* path, uint8_t value)
{
    auto* p = vm->propertyBoolean(path);
    if (p == nullptr)
    {
        shim_set_error("view-model bool property not found");
        return 1;
    }
    p->value(value != 0);
    return RIVE_OK;
}
RiveStatus vmi_get_bool(ViewModelInstanceRuntime* vm, const char* path, uint8_t* out)
{
    if (out == nullptr)
    {
        shim_set_error("out pointer is null");
        return 1;
    }
    auto* p = vm->propertyBoolean(path);
    if (p == nullptr)
    {
        shim_set_error("view-model bool property not found");
        return 1;
    }
    *out = p->value() ? 1 : 0;
    return RIVE_OK;
}
RiveStatus vmi_set_color(ViewModelInstanceRuntime* vm, const char* path, uint32_t argb)
{
    auto* p = vm->propertyColor(path);
    if (p == nullptr)
    {
        shim_set_error("view-model color property not found");
        return 1;
    }
    p->value(static_cast<int>(argb));
    return RIVE_OK;
}
RiveStatus vmi_get_color(ViewModelInstanceRuntime* vm, const char* path, uint32_t* out)
{
    if (out == nullptr)
    {
        shim_set_error("out pointer is null");
        return 1;
    }
    auto* p = vm->propertyColor(path);
    if (p == nullptr)
    {
        shim_set_error("view-model color property not found");
        return 1;
    }
    *out = static_cast<uint32_t>(p->value());
    return RIVE_OK;
}
RiveStatus vmi_set_string(ViewModelInstanceRuntime* vm, const char* path, const char* value)
{
    if (value == nullptr)
    {
        shim_set_error("view-model string value is null");
        return 1;
    }
    auto* p = vm->propertyString(path);
    if (p == nullptr)
    {
        shim_set_error("view-model string property not found");
        return 1;
    }
    p->value(std::string(value));
    return RIVE_OK;
}
RiveStatus vmi_get_string(ViewModelInstanceRuntime* vm, const char* path, char* buf,
                          size_t cap, size_t* out_len)
{
    auto* p = vm->propertyString(path);
    if (p == nullptr)
    {
        shim_set_error("view-model string property not found");
        return 1;
    }
    copy_to_caller(p->value(), buf, cap, out_len);
    return RIVE_OK;
}
RiveStatus vmi_get_enum_index(ViewModelInstanceRuntime* vm, const char* path, uint32_t* out)
{
    if (out == nullptr)
    {
        shim_set_error("out pointer is null");
        return 1;
    }
    auto* p = vm->propertyEnum(path);
    if (p == nullptr)
    {
        shim_set_error("view-model enum property not found");
        return 1;
    }
    *out = p->valueIndex();
    return RIVE_OK;
}
RiveStatus vmi_set_enum_index(ViewModelInstanceRuntime* vm, const char* path, uint32_t index)
{
    auto* p = vm->propertyEnum(path);
    if (p == nullptr)
    {
        shim_set_error("view-model enum property not found");
        return 1;
    }
    p->valueIndex(index);
    return RIVE_OK;
}
RiveStatus vmi_set_enum_name(ViewModelInstanceRuntime* vm, const char* path, const char* name)
{
    if (name == nullptr)
    {
        shim_set_error("view-model enum name is null");
        return 1;
    }
    auto* p = vm->propertyEnum(path);
    if (p == nullptr)
    {
        shim_set_error("view-model enum property not found");
        return 1;
    }
    p->value(std::string(name));
    return RIVE_OK;
}
RiveStatus vmi_trigger(ViewModelInstanceRuntime* vm, const char* path)
{
    auto* p = vm->propertyTrigger(path);
    if (p == nullptr)
    {
        shim_set_error("view-model trigger property not found");
        return 1;
    }
    p->trigger();
    return RIVE_OK;
}
// Binds a decoded image to an image (assetImage) property. `image == nullptr`
// clears the binding (rive's value(nullptr)). The runtime takes its OWN ref on the
// RenderImage, so the caller's RiveImage may be freed afterwards. The image must
// have been decoded by the SAME render context the artboard renders with (the safe
// layer enforces this); the shim only sees an already-decoded RenderImage.
RiveStatus vmi_set_image(ViewModelInstanceRuntime* vm, const char* path, RiveImage* image)
{
    auto* p = vm->propertyImage(path);
    if (p == nullptr)
    {
        shim_set_error("view-model image property not found");
        return 1;
    }
    p->value(image != nullptr ? image->image.get() : nullptr);
    return RIVE_OK;
}

// Binds a file-sourced BindableArtboard to an artboard (propertyArtboard) property.
// `bindable == nullptr` clears the binding (rive's value(nullptr)). The runtime
// takes its OWN ref on the BindableArtboard, so the caller's RiveBindableArtboard
// may be freed afterwards. Mirrors vmi_set_image for the artboard value type.
RiveStatus vmi_set_artboard(ViewModelInstanceRuntime* vm, const char* path,
                            RiveBindableArtboard* bindable)
{
    auto* p = vm->propertyArtboard(path);
    if (p == nullptr)
    {
        shim_set_error("view-model artboard property not found");
        return 1;
    }
    p->value(bindable != nullptr ? bindable->bindable : nullptr);
    return RIVE_OK;
}

// Schema introspection on a resolved runtime (shared by both surfaces).
RiveStatus vmi_property_at_core(ViewModelInstanceRuntime* vm, uint32_t index,
                                char* name_buf, size_t cap, size_t* out_len,
                                int* out_type)
{
    std::vector<rive::PropertyData> props = vm->properties();
    if (index >= props.size())
    {
        shim_set_error("view-model property index out of range");
        return 1;
    }
    const std::string& name = props[index].name;
    if (out_len != nullptr)
        *out_len = name.size();
    if (out_type != nullptr)
        *out_type = static_cast<int>(props[index].type);
    // Copy up to `cap` bytes (no NUL terminator; the caller slices to *out_len).
    if (name_buf != nullptr && cap > 0)
    {
        const size_t n = name.size() < cap ? name.size() : cap;
        std::memcpy(name_buf, name.data(), n);
    }
    return RIVE_OK;
}

// Type-agnostic CHANGE / TRIGGER observation — the modern, non-deprecated
// replacement for events read-back (Rive deprecated runtime event listening; see
// docs/feature-support.md). `property(path)` returns the value runtime for ANY
// property type, creating + caching + SUBSCRIBING it as a dependent on first call.
// `flushChanges()` returns true ONCE per change/fire the rig produced on the last
// advance, then resets. Usage: prime once at setup (so the wrapper is subscribed
// before the first advance), then poll each frame AFTER advance. Works uniformly
// for triggers (a fire) and scalar properties (a value change).
RiveStatus vmi_flush_changed(ViewModelInstanceRuntime* vm, const char* path, uint8_t* out)
{
    if (out == nullptr)
    {
        shim_set_error("out pointer is null");
        return 1;
    }
    auto* p = vm->property(path);
    if (p == nullptr)
    {
        shim_set_error("view-model property not found");
        return 1;
    }
    *out = p->flushChanges() ? 1 : 0;
    return RIVE_OK;
}

} // namespace

// ===========================================================================
// Artboard-rooted ABI — flat path on the artboard's root view model. Thin
// wrappers over vmi-core (behavior is identical to the pre-refactor bodies; the
// byte-identical regression suite is the proof).
// ===========================================================================

extern "C" RiveStatus rive_artboard_vm_set_number(RiveArtboard* artboard,
                                                  const char* path, float value)
{
    ViewModelInstanceRuntime* vm = root_vm_path(artboard, path);
    return vm == nullptr ? 1 : vmi_set_number(vm, path, value);
}

extern "C" RiveStatus rive_artboard_vm_get_number(RiveArtboard* artboard,
                                                  const char* path, float* out)
{
    ViewModelInstanceRuntime* vm = root_vm_path(artboard, path);
    return vm == nullptr ? 1 : vmi_get_number(vm, path, out);
}

extern "C" RiveStatus rive_artboard_vm_set_bool(RiveArtboard* artboard,
                                                const char* path, uint8_t value)
{
    ViewModelInstanceRuntime* vm = root_vm_path(artboard, path);
    return vm == nullptr ? 1 : vmi_set_bool(vm, path, value);
}

extern "C" RiveStatus rive_artboard_vm_get_bool(RiveArtboard* artboard,
                                                const char* path, uint8_t* out)
{
    ViewModelInstanceRuntime* vm = root_vm_path(artboard, path);
    return vm == nullptr ? 1 : vmi_get_bool(vm, path, out);
}

extern "C" RiveStatus rive_artboard_vm_fire_trigger(RiveArtboard* artboard,
                                                    const char* path)
{
    ViewModelInstanceRuntime* vm = root_vm_path(artboard, path);
    return vm == nullptr ? 1 : vmi_trigger(vm, path);
}

extern "C" RiveStatus rive_artboard_vm_set_color(RiveArtboard* artboard,
                                                 const char* path, uint32_t argb)
{
    ViewModelInstanceRuntime* vm = root_vm_path(artboard, path);
    return vm == nullptr ? 1 : vmi_set_color(vm, path, argb);
}

extern "C" RiveStatus rive_artboard_vm_get_color(RiveArtboard* artboard,
                                                 const char* path, uint32_t* out)
{
    ViewModelInstanceRuntime* vm = root_vm_path(artboard, path);
    return vm == nullptr ? 1 : vmi_get_color(vm, path, out);
}

extern "C" RiveStatus rive_artboard_vm_set_string(RiveArtboard* artboard,
                                                  const char* path, const char* value)
{
    ViewModelInstanceRuntime* vm = root_vm_path(artboard, path);
    return vm == nullptr ? 1 : vmi_set_string(vm, path, value);
}

extern "C" RiveStatus rive_artboard_vm_get_string(RiveArtboard* artboard,
                                                  const char* path, char* buf,
                                                  size_t cap, size_t* out_len)
{
    ViewModelInstanceRuntime* vm = root_vm_path(artboard, path);
    return vm == nullptr ? 1 : vmi_get_string(vm, path, buf, cap, out_len);
}

extern "C" RiveStatus rive_artboard_vm_set_enum_index(RiveArtboard* artboard,
                                                      const char* path, uint32_t index)
{
    ViewModelInstanceRuntime* vm = root_vm_path(artboard, path);
    return vm == nullptr ? 1 : vmi_set_enum_index(vm, path, index);
}

extern "C" RiveStatus rive_artboard_vm_get_enum_index(RiveArtboard* artboard,
                                                      const char* path, uint32_t* out)
{
    ViewModelInstanceRuntime* vm = root_vm_path(artboard, path);
    return vm == nullptr ? 1 : vmi_get_enum_index(vm, path, out);
}

extern "C" RiveStatus rive_artboard_vm_set_enum_name(RiveArtboard* artboard,
                                                     const char* path, const char* name)
{
    ViewModelInstanceRuntime* vm = root_vm_path(artboard, path);
    return vm == nullptr ? 1 : vmi_set_enum_name(vm, path, name);
}

extern "C" RiveStatus rive_artboard_vm_enum_value_count(RiveArtboard* artboard,
                                                        const char* path, uint32_t* out)
{
    ViewModelInstanceRuntime* vm = root_vm_path(artboard, path);
    if (vm == nullptr)
        return 1;
    if (out == nullptr)
    {
        shim_set_error("out pointer is null");
        return 1;
    }
    auto* p = vm->propertyEnum(path);
    if (p == nullptr)
    {
        shim_set_error("view-model enum property not found");
        return 1;
    }
    *out = static_cast<uint32_t>(p->values().size());
    return RIVE_OK;
}

extern "C" RiveStatus rive_artboard_vm_enum_value_at(RiveArtboard* artboard,
                                                     const char* path, uint32_t index,
                                                     char* buf, size_t cap, size_t* out_len)
{
    ViewModelInstanceRuntime* vm = root_vm_path(artboard, path);
    if (vm == nullptr)
        return 1;
    auto* p = vm->propertyEnum(path);
    if (p == nullptr)
    {
        shim_set_error("view-model enum property not found");
        return 1;
    }
    std::vector<std::string> vals = p->values();
    if (index >= vals.size())
    {
        shim_set_error("view-model enum value index out of range");
        return 1;
    }
    copy_to_caller(vals[index], buf, cap, out_len);
    return RIVE_OK;
}

extern "C" uint32_t rive_artboard_vm_property_count(RiveArtboard* artboard)
{
    if (artboard == nullptr || artboard->vmRuntime == nullptr)
        return 0;
    return static_cast<uint32_t>(artboard->vmRuntime->properties().size());
}

extern "C" RiveStatus rive_artboard_vm_property_at(RiveArtboard* artboard,
                                                   uint32_t index,
                                                   char* name_buf, size_t cap,
                                                   size_t* out_len, int* out_type)
{
    ViewModelInstanceRuntime* vm = root_vm(artboard);
    if (vm == nullptr)
        return 1;
    return vmi_property_at_core(vm, index, name_buf, cap, out_len, out_type);
}

// Change / trigger observation (modern events replacement — see vmi_flush_changed).
extern "C" RiveStatus rive_artboard_vm_flush_changed(RiveArtboard* artboard,
                                                     const char* path, uint8_t* out)
{
    ViewModelInstanceRuntime* vm = root_vm_path(artboard, path);
    return vm == nullptr ? 1 : vmi_flush_changed(vm, path, out);
}

// Binds a decoded image to a root-VM image property (`/` reaches nested VMs).
// `image == nullptr` clears it. See vmi_set_image for the same-context requirement.
extern "C" RiveStatus rive_artboard_vm_set_image(RiveArtboard* artboard,
                                                 const char* path, RiveImage* image)
{
    ViewModelInstanceRuntime* vm = root_vm_path(artboard, path);
    return vm == nullptr ? 1 : vmi_set_image(vm, path, image);
}

// Binds a file-sourced BindableArtboard to a root-VM artboard property (`/` reaches
// nested VMs). `bindable == nullptr` clears it. See rive_file_bindable_artboard_*
// for creating the value source. Mirrors rive_artboard_vm_set_image.
extern "C" RiveStatus rive_artboard_vm_set_artboard(RiveArtboard* artboard,
                                                    const char* path,
                                                    RiveBindableArtboard* bindable)
{
    ViewModelInstanceRuntime* vm = root_vm_path(artboard, path);
    return vm == nullptr ? 1 : vmi_set_artboard(vm, path, bindable);
}

// ===========================================================================
// Handle-based ABI — operate on a RiveViewModelInstance* (root, nested VM, or
// list item). Enables nested-VM introspection + list access the flat path can't
// express. Handles are borrowed (owned by rive's caches under the root vmRuntime;
// valid while the artboard lives and the addressed list is unmodified).
// ===========================================================================

// The artboard's root view-model instance as a handle (null + error if none).
extern "C" RiveViewModelInstance* rive_artboard_vm_root(RiveArtboard* artboard)
{
    return as_handle(root_vm(artboard));
}

// A nested view-model instance at `path` (relative to `handle`; `/` descends).
// Null + error if the path is not a view-model property.
extern "C" RiveViewModelInstance* rive_vmi_property_view_model(
    RiveViewModelInstance* handle, const char* path)
{
    ViewModelInstanceRuntime* vm = vmi_path(handle, path);
    if (vm == nullptr)
        return nullptr;
    rive::rcp<ViewModelInstanceRuntime> nested = vm->propertyViewModel(path);
    if (nested == nullptr)
    {
        shim_set_error("view-model nested view-model property not found");
        return nullptr;
    }
    // The runtime caches `nested` in this instance's m_viewModelInstances, so the
    // pointee outlives our local rcp (lifetime tied to `handle`'s instance).
    return as_handle(nested.get());
}

// Number of elements in the list property at `path`.
extern "C" RiveStatus rive_vmi_list_size(RiveViewModelInstance* handle,
                                         const char* path, uint32_t* out)
{
    ViewModelInstanceRuntime* vm = vmi_path(handle, path);
    if (vm == nullptr)
        return 1;
    if (out == nullptr)
    {
        shim_set_error("out pointer is null");
        return 1;
    }
    auto* list = vm->propertyList(path);
    if (list == nullptr)
    {
        shim_set_error("view-model list property not found");
        return 1;
    }
    *out = static_cast<uint32_t>(list->size());
    return RIVE_OK;
}

// The list item at `index` as a view-model-instance handle. Null + error if the
// path is not a list, the index is out of range, or the item has no instance.
extern "C" RiveViewModelInstance* rive_vmi_list_instance_at(
    RiveViewModelInstance* handle, const char* path, uint32_t index)
{
    ViewModelInstanceRuntime* vm = vmi_path(handle, path);
    if (vm == nullptr)
        return nullptr;
    auto* list = vm->propertyList(path);
    if (list == nullptr)
    {
        shim_set_error("view-model list property not found");
        return nullptr;
    }
    // instanceAt bounds-checks (returns null for oob / itemless); cache keeps the
    // returned instance alive (lifetime tied to the list, hence to `handle`).
    rive::rcp<ViewModelInstanceRuntime> item = list->instanceAt(static_cast<int>(index));
    if (item == nullptr)
    {
        shim_set_error("view-model list index out of range or item empty");
        return nullptr;
    }
    return as_handle(item.get());
}

// Number of properties on this instance (0 if the handle is null).
extern "C" uint32_t rive_vmi_property_count(RiveViewModelInstance* handle)
{
    if (handle == nullptr)
        return 0;
    return static_cast<uint32_t>(as_vmi(handle)->properties().size());
}

// The (name, DataType ordinal) of the property at `index` (two-call buffer).
extern "C" RiveStatus rive_vmi_property_at(RiveViewModelInstance* handle,
                                           uint32_t index, char* name_buf, size_t cap,
                                           size_t* out_len, int* out_type)
{
    if (handle == nullptr)
    {
        shim_set_error("view-model instance handle is null");
        return 1;
    }
    return vmi_property_at_core(as_vmi(handle), index, name_buf, cap, out_len, out_type);
}

// ---- handle reads (number / bool / color / string / enum index) ----

extern "C" RiveStatus rive_vmi_get_number(RiveViewModelInstance* handle,
                                          const char* path, float* out)
{
    ViewModelInstanceRuntime* vm = vmi_path(handle, path);
    return vm == nullptr ? 1 : vmi_get_number(vm, path, out);
}
extern "C" RiveStatus rive_vmi_get_bool(RiveViewModelInstance* handle,
                                        const char* path, uint8_t* out)
{
    ViewModelInstanceRuntime* vm = vmi_path(handle, path);
    return vm == nullptr ? 1 : vmi_get_bool(vm, path, out);
}
extern "C" RiveStatus rive_vmi_get_color(RiveViewModelInstance* handle,
                                         const char* path, uint32_t* out)
{
    ViewModelInstanceRuntime* vm = vmi_path(handle, path);
    return vm == nullptr ? 1 : vmi_get_color(vm, path, out);
}
extern "C" RiveStatus rive_vmi_get_string(RiveViewModelInstance* handle,
                                          const char* path, char* buf, size_t cap,
                                          size_t* out_len)
{
    ViewModelInstanceRuntime* vm = vmi_path(handle, path);
    return vm == nullptr ? 1 : vmi_get_string(vm, path, buf, cap, out_len);
}
extern "C" RiveStatus rive_vmi_get_enum_index(RiveViewModelInstance* handle,
                                              const char* path, uint32_t* out)
{
    ViewModelInstanceRuntime* vm = vmi_path(handle, path);
    return vm == nullptr ? 1 : vmi_get_enum_index(vm, path, out);
}
extern "C" RiveStatus rive_vmi_flush_changed(RiveViewModelInstance* handle,
                                             const char* path, uint8_t* out)
{
    ViewModelInstanceRuntime* vm = vmi_path(handle, path);
    return vm == nullptr ? 1 : vmi_flush_changed(vm, path, out);
}

// ---- handle writes (number / bool / color / string / enum / trigger) ----
// Mirror the handle reads onto the shared set cores, so a caller can write into
// a nested view model or a LIST ITEM (which the flat artboard-rooted path can't
// address — the native resolver can't index lists). Same borrowed-handle rules.

extern "C" RiveStatus rive_vmi_set_number(RiveViewModelInstance* handle,
                                          const char* path, float value)
{
    ViewModelInstanceRuntime* vm = vmi_path(handle, path);
    return vm == nullptr ? 1 : vmi_set_number(vm, path, value);
}
extern "C" RiveStatus rive_vmi_set_bool(RiveViewModelInstance* handle,
                                        const char* path, uint8_t value)
{
    ViewModelInstanceRuntime* vm = vmi_path(handle, path);
    return vm == nullptr ? 1 : vmi_set_bool(vm, path, value);
}
extern "C" RiveStatus rive_vmi_set_color(RiveViewModelInstance* handle,
                                         const char* path, uint32_t argb)
{
    ViewModelInstanceRuntime* vm = vmi_path(handle, path);
    return vm == nullptr ? 1 : vmi_set_color(vm, path, argb);
}
extern "C" RiveStatus rive_vmi_set_string(RiveViewModelInstance* handle,
                                          const char* path, const char* value)
{
    ViewModelInstanceRuntime* vm = vmi_path(handle, path);
    return vm == nullptr ? 1 : vmi_set_string(vm, path, value);
}
extern "C" RiveStatus rive_vmi_set_enum_index(RiveViewModelInstance* handle,
                                              const char* path, uint32_t index)
{
    ViewModelInstanceRuntime* vm = vmi_path(handle, path);
    return vm == nullptr ? 1 : vmi_set_enum_index(vm, path, index);
}
extern "C" RiveStatus rive_vmi_set_enum_name(RiveViewModelInstance* handle,
                                             const char* path, const char* name)
{
    ViewModelInstanceRuntime* vm = vmi_path(handle, path);
    return vm == nullptr ? 1 : vmi_set_enum_name(vm, path, name);
}
extern "C" RiveStatus rive_vmi_fire_trigger(RiveViewModelInstance* handle, const char* path)
{
    ViewModelInstanceRuntime* vm = vmi_path(handle, path);
    return vm == nullptr ? 1 : vmi_trigger(vm, path);
}
// Bind a decoded image into a nested VM or a LIST ITEM's image property (which the
// flat artboard-rooted path can't address). `image == nullptr` clears it.
extern "C" RiveStatus rive_vmi_set_image(RiveViewModelInstance* handle,
                                         const char* path, RiveImage* image)
{
    ViewModelInstanceRuntime* vm = vmi_path(handle, path);
    return vm == nullptr ? 1 : vmi_set_image(vm, path, image);
}
// Bind a file-sourced BindableArtboard into a nested VM or a LIST ITEM's artboard
// property (which the flat artboard-rooted path can't address). `bindable == nullptr`
// clears it.
extern "C" RiveStatus rive_vmi_set_artboard(RiveViewModelInstance* handle,
                                            const char* path, RiveBindableArtboard* bindable)
{
    ViewModelInstanceRuntime* vm = vmi_path(handle, path);
    return vm == nullptr ? 1 : vmi_set_artboard(vm, path, bindable);
}

// ===========================================================================
// View-model INSTANCE construction + LIST structural mutation + VM-ref assignment.
//
// A RiveViewModelRuntime* is a view-model DEFINITION (rive::ViewModelRuntime),
// borrowed + owned by the File (valid while the artboard's File lives). Reached from
// an artboard handle via its stashed File*, it mints fresh, caller-OWNED instances
// (RiveOwnedVmInstance) that can be populated (borrow into the get/set surface), then
// added to a list (rive_vmi_list_add*) or assigned to a VM-reference property
// (rive_vmi_replace_view_model) — after which the list/parent co-owns them, so the
// owned wrapper may be destroyed. Every mutator marks data-bindings dirty, so a
// structural edit takes effect on the next advance. Indices are positional and shift
// on add/remove/swap. The void-returning natives (remove-at/swap) silently no-op out
// of range, so we bounds-check against size() to surface a useful error.
// ===========================================================================

namespace {
using rive::ViewModelRuntime;

inline ViewModelRuntime* as_vmr(RiveViewModelRuntime* h)
{
    return reinterpret_cast<ViewModelRuntime*>(h);
}
inline RiveViewModelRuntime* as_vmr_handle(ViewModelRuntime* p)
{
    return reinterpret_cast<RiveViewModelRuntime*>(p);
}

// Resolve a (handle, path) to its list runtime, or null (+ error) if the handle is
// null, the path is null, or `path` is not a list property. Mirrors the resolution
// the existing rive_vmi_list_size / _instance_at inline.
rive::ViewModelInstanceListRuntime* vmi_list(RiveViewModelInstance* handle, const char* path)
{
    ViewModelInstanceRuntime* vm = vmi_path(handle, path);
    if (vm == nullptr)
        return nullptr;
    auto* list = vm->propertyList(path);
    if (list == nullptr)
        shim_set_error("view-model list property not found");
    return list;
}

// Wrap a freshly-created instance rcp in an owned handle, or null (+ error) if the
// creation returned null (invalid VM) or on OOM. Shared by the four create verbs.
RiveOwnedVmInstance* wrap_owned(rive::rcp<ViewModelInstanceRuntime> inst, const char* whatFailed)
{
    if (inst == nullptr)
    {
        shim_set_error(whatFailed);
        return nullptr;
    }
    auto* h = new (std::nothrow) RiveOwnedVmInstance{std::move(inst)};
    if (h == nullptr)
        shim_set_error("out of memory allocating RiveOwnedVmInstance");
    return h;
}
} // namespace

// ---- Artboard -> view-model DEFINITION (rive::ViewModelRuntime) access ----

// Number of view-model definitions in the artboard's File (0 if no file).
extern "C" uint32_t rive_artboard_view_model_count(RiveArtboard* artboard)
{
    if (artboard == nullptr || artboard->file == nullptr)
        return 0;
    return static_cast<uint32_t>(artboard->file->viewModelCount());
}

// The view-model definition named `name`, or null + error if the artboard has no
// file or no such view model.
extern "C" RiveViewModelRuntime* rive_artboard_view_model_by_name(RiveArtboard* artboard,
                                                                  const char* name)
{
    if (artboard == nullptr || artboard->file == nullptr || name == nullptr)
    {
        shim_set_error("artboard has no file, or the view-model name is null");
        return nullptr;
    }
    ViewModelRuntime* vmr = artboard->file->viewModelByName(name);
    if (vmr == nullptr)
        shim_set_error("no view-model definition with that name");
    return as_vmr_handle(vmr);
}

// The view-model definition at `index`, or null + error if out of range.
extern "C" RiveViewModelRuntime* rive_artboard_view_model_by_index(RiveArtboard* artboard,
                                                                   uint32_t index)
{
    if (artboard == nullptr || artboard->file == nullptr)
    {
        shim_set_error("artboard has no file");
        return nullptr;
    }
    ViewModelRuntime* vmr = artboard->file->viewModelByIndex(index);
    if (vmr == nullptr)
        shim_set_error("view-model definition index out of range");
    return as_vmr_handle(vmr);
}

// The view-model definition bound to THIS artboard (the type of its own root VM), or
// null + error if the artboard has no linked view model.
extern "C" RiveViewModelRuntime* rive_artboard_default_view_model(RiveArtboard* artboard)
{
    if (artboard == nullptr || artboard->file == nullptr || artboard->inst() == nullptr)
    {
        shim_set_error("artboard has no file or instance");
        return nullptr;
    }
    ViewModelRuntime* vmr = artboard->file->defaultArtboardViewModel(artboard->inst());
    if (vmr == nullptr)
        shim_set_error("artboard has no linked view model");
    return as_vmr_handle(vmr);
}

// ---- view-model DEFINITION introspection ----

// The definition's name (two-call buffer protocol).
extern "C" RiveStatus rive_view_model_name(RiveViewModelRuntime* vmr, char* buf, size_t cap,
                                           size_t* out_len)
{
    if (vmr == nullptr)
    {
        shim_set_error("view-model runtime handle is null");
        return 1;
    }
    copy_to_caller(as_vmr(vmr)->name(), buf, cap, out_len);
    return RIVE_OK;
}

// Number of editor-authored named instances of this definition (the names
// createInstanceFromName / _index can clone).
extern "C" uint32_t rive_view_model_instance_count(RiveViewModelRuntime* vmr)
{
    if (vmr == nullptr)
        return 0;
    return static_cast<uint32_t>(as_vmr(vmr)->instanceCount());
}

// The name of the editor instance at `index` (two-call buffer protocol).
extern "C" RiveStatus rive_view_model_instance_name_at(RiveViewModelRuntime* vmr, uint32_t index,
                                                       char* buf, size_t cap, size_t* out_len)
{
    if (vmr == nullptr)
    {
        shim_set_error("view-model runtime handle is null");
        return 1;
    }
    std::vector<std::string> names = as_vmr(vmr)->instanceNames();
    if (index >= names.size())
    {
        shim_set_error("view-model instance name index out of range");
        return 1;
    }
    copy_to_caller(names[index], buf, cap, out_len);
    return RIVE_OK;
}

// ---- mint fresh, caller-OWNED instances ----

// A blank instance (all default property values).
extern "C" RiveOwnedVmInstance* rive_view_model_create_instance(RiveViewModelRuntime* vmr)
{
    if (vmr == nullptr)
    {
        shim_set_error("view-model runtime handle is null");
        return nullptr;
    }
    return wrap_owned(as_vmr(vmr)->createInstance(), "createInstance returned null");
}

// The editor's default instance (falls back to a blank instance if none authored).
extern "C" RiveOwnedVmInstance* rive_view_model_create_default_instance(RiveViewModelRuntime* vmr)
{
    if (vmr == nullptr)
    {
        shim_set_error("view-model runtime handle is null");
        return nullptr;
    }
    return wrap_owned(as_vmr(vmr)->createDefaultInstance(), "createDefaultInstance returned null");
}

// A clone of the editor instance named `name` (null + error if not found/exported).
extern "C" RiveOwnedVmInstance* rive_view_model_create_instance_from_name(RiveViewModelRuntime* vmr,
                                                                          const char* name)
{
    if (vmr == nullptr || name == nullptr)
    {
        shim_set_error("view-model runtime handle or instance name is null");
        return nullptr;
    }
    return wrap_owned(as_vmr(vmr)->createInstanceFromName(std::string(name)),
                      "no editor view-model instance with that name");
}

// A clone of the editor instance at `index` (null + error if out of range).
extern "C" RiveOwnedVmInstance* rive_view_model_create_instance_from_index(RiveViewModelRuntime* vmr,
                                                                           uint32_t index)
{
    if (vmr == nullptr)
    {
        shim_set_error("view-model runtime handle is null");
        return nullptr;
    }
    return wrap_owned(as_vmr(vmr)->createInstanceFromIndex(index),
                      "view-model instance index out of range");
}

// ---- owned-instance borrow + destroy ----

// Borrow the owned instance as a RiveViewModelInstance* so the get/set surface can
// populate it before it is added to a list / assigned. The borrow is valid while the
// owned handle lives (and, after an add/assign, while the list/parent holds it).
extern "C" RiveViewModelInstance* rive_owned_vmi_borrow(RiveOwnedVmInstance* owned)
{
    if (owned == nullptr)
    {
        shim_set_error("owned view-model instance handle is null");
        return nullptr;
    }
    return as_handle(owned->inst.get());
}

// Release the caller's ref on an owned instance. Safe to call after adding it to a
// list / assigning it (the list/parent co-owns it, so it survives); if it was never
// added, this frees it.
extern "C" void rive_owned_vmi_destroy(RiveOwnedVmInstance* owned)
{
    if (owned == nullptr)
        return;
    owned->inst = nullptr; // drop our rcp; the list/parent keeps it alive if added
    delete owned;
}

// ---- LIST structural mutation (on the list-owning handle + path) ----

// Append `item` to the end of the list.
extern "C" RiveStatus rive_vmi_list_add(RiveViewModelInstance* handle, const char* path,
                                        RiveViewModelInstance* item)
{
    auto* list = vmi_list(handle, path);
    if (list == nullptr)
        return 1;
    if (item == nullptr)
    {
        shim_set_error("list item to add is null");
        return 1;
    }
    list->addInstance(as_vmi(item));
    return RIVE_OK;
}

// Insert `item` at `index` (valid range [0, size]; append allowed at index==size).
extern "C" RiveStatus rive_vmi_list_add_at(RiveViewModelInstance* handle, const char* path,
                                           RiveViewModelInstance* item, uint32_t index)
{
    auto* list = vmi_list(handle, path);
    if (list == nullptr)
        return 1;
    if (item == nullptr)
    {
        shim_set_error("list item to add is null");
        return 1;
    }
    if (!list->addInstanceAt(as_vmi(item), static_cast<int>(index)))
    {
        shim_set_error("list insert index out of range");
        return 1;
    }
    return RIVE_OK;
}

// Remove EVERY occurrence of `item` (matched by underlying instance).
extern "C" RiveStatus rive_vmi_list_remove(RiveViewModelInstance* handle, const char* path,
                                           RiveViewModelInstance* item)
{
    auto* list = vmi_list(handle, path);
    if (list == nullptr)
        return 1;
    if (item == nullptr)
    {
        shim_set_error("list item to remove is null");
        return 1;
    }
    list->removeInstance(as_vmi(item));
    return RIVE_OK;
}

// Remove the item at `index` (error if out of range — the native no-ops silently).
extern "C" RiveStatus rive_vmi_list_remove_at(RiveViewModelInstance* handle, const char* path,
                                              uint32_t index)
{
    auto* list = vmi_list(handle, path);
    if (list == nullptr)
        return 1;
    if (index >= list->size())
    {
        shim_set_error("list remove index out of range");
        return 1;
    }
    list->removeInstanceAt(static_cast<int>(index));
    return RIVE_OK;
}

// Swap the items at `a` and `b` (error if either is out of range).
extern "C" RiveStatus rive_vmi_list_swap(RiveViewModelInstance* handle, const char* path,
                                         uint32_t a, uint32_t b)
{
    auto* list = vmi_list(handle, path);
    if (list == nullptr)
        return 1;
    const size_t n = list->size();
    if (a >= n || b >= n)
    {
        shim_set_error("list swap index out of range");
        return 1;
    }
    list->swap(a, b);
    return RIVE_OK;
}

// Remove all items, leaving the list empty.
extern "C" RiveStatus rive_vmi_list_clear(RiveViewModelInstance* handle, const char* path)
{
    auto* list = vmi_list(handle, path);
    if (list == nullptr)
        return 1;
    list->removeAllInstances();
    return RIVE_OK;
}

// ---- VM-reference property assignment ----

// Assign `value` to the view-model-reference property at `path` (`/` descends). Fails
// (error) if `path` is not a VM-reference property or `value`'s view-model TYPE does
// not match the property's referenced type (rive enforces the id match).
extern "C" RiveStatus rive_vmi_replace_view_model(RiveViewModelInstance* handle, const char* path,
                                                  RiveViewModelInstance* value)
{
    ViewModelInstanceRuntime* vm = vmi_path(handle, path);
    if (vm == nullptr)
        return 1;
    if (value == nullptr)
    {
        shim_set_error("replacement view-model instance is null");
        return 1;
    }
    if (!vm->replaceViewModel(path, as_vmi(value)))
    {
        shim_set_error("replaceViewModel failed (not a view-model property or type mismatch)");
        return 1;
    }
    return RIVE_OK;
}
