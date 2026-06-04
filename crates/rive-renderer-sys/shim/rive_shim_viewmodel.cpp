/*
 * rive_shim_viewmodel.cpp — view-model data binding C ABI.
 *
 * Per-feature shim TU (see rive_shim_internal.hpp + docs/feature-support.md):
 * get/set named view-model properties on an artboard's bound DEFAULT view-model
 * instance, via the ViewModelInstanceRuntime wrapper held on RiveArtboard. The
 * API mirrors docs/cpp/data-binding.mdx.
 *
 * Two surfaces share one core:
 *   - artboard-rooted (`rive_artboard_vm_*`): flat path on the artboard's root
 *     view model; `/` reaches into named nested view models. number/bool/trigger/
 *     color/string/enum get+set + top-level schema introspection.
 *   - handle-based (`rive_vmi_*`): an opaque RiveViewModelInstance* handle for a
 *     view-model INSTANCE (the root, a nested VM, or a list item). Adds nested-VM
 *     introspection (recurse `propertyViewModel`), list size + item access
 *     (`propertyList`/`instanceAt` — the native path resolver can't index lists),
 *     and reads. Handles are BORROWED: they alias instances owned by rive's caches
 *     under the root `vmRuntime`, so they are valid only while the artboard lives
 *     and the addressed list is not mutated. Reads only this slice (writes go
 *     through the artboard-rooted setters; list mutation + image/artboard refs are
 *     deferred — see docs/feature-support.md).
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
#include "rive/viewmodel/runtime/viewmodel_runtime.hpp" // PropertyData

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
    if (vm == nullptr)
        return 1;
    auto* p = vm->propertyTrigger(path);
    if (p == nullptr)
    {
        shim_set_error("view-model trigger property not found");
        return 1;
    }
    p->trigger();
    return RIVE_OK;
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
    if (vm == nullptr)
        return 1;
    auto* p = vm->propertyEnum(path);
    if (p == nullptr)
    {
        shim_set_error("view-model enum property not found");
        return 1;
    }
    p->valueIndex(index);
    return RIVE_OK;
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
    if (vm == nullptr)
        return 1;
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
