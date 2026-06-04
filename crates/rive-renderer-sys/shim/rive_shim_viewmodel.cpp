/*
 * rive_shim_viewmodel.cpp — view-model data binding C ABI.
 *
 * Per-feature shim TU (see rive_shim_internal.hpp + docs/feature-support.md):
 * get/set named view-model properties on an artboard's bound DEFAULT view-model
 * instance, via the ViewModelInstanceRuntime wrapper held on RiveArtboard. The
 * API mirrors docs/cpp/data-binding.mdx. Slice 1: number, bool, trigger, and
 * schema introspection. (color/string/enum follow.)
 */
#include "rive_shim_internal.hpp"

#include <cstring>

#include "rive/viewmodel/runtime/viewmodel_instance_number_runtime.hpp"
#include "rive/viewmodel/runtime/viewmodel_instance_boolean_runtime.hpp"
#include "rive/viewmodel/runtime/viewmodel_instance_trigger_runtime.hpp"
#include "rive/viewmodel/runtime/viewmodel_runtime.hpp" // PropertyData

namespace {

// Shared guard: the artboard must exist and carry a view-model runtime, and the
// path must be non-null. Returns the runtime (non-null) or null after an error.
rive::ViewModelInstanceRuntime* vm_of(RiveArtboard* artboard, const char* path)
{
    if (artboard == nullptr || artboard->vmRuntime == nullptr)
    {
        shim_set_error("artboard has no view model");
        return nullptr;
    }
    if (path == nullptr)
    {
        shim_set_error("view-model property path is null");
        return nullptr;
    }
    return artboard->vmRuntime.get();
}

} // namespace

extern "C" RiveStatus rive_artboard_vm_set_number(RiveArtboard* artboard,
                                                  const char* path, float value)
{
    rive::ViewModelInstanceRuntime* vm = vm_of(artboard, path);
    if (vm == nullptr)
        return 1;
    auto* p = vm->propertyNumber(path);
    if (p == nullptr)
    {
        shim_set_error("view-model number property not found");
        return 1;
    }
    p->value(value);
    return RIVE_OK;
}

extern "C" RiveStatus rive_artboard_vm_get_number(RiveArtboard* artboard,
                                                  const char* path, float* out)
{
    rive::ViewModelInstanceRuntime* vm = vm_of(artboard, path);
    if (vm == nullptr)
        return 1;
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

extern "C" RiveStatus rive_artboard_vm_set_bool(RiveArtboard* artboard,
                                                const char* path, uint8_t value)
{
    rive::ViewModelInstanceRuntime* vm = vm_of(artboard, path);
    if (vm == nullptr)
        return 1;
    auto* p = vm->propertyBoolean(path);
    if (p == nullptr)
    {
        shim_set_error("view-model bool property not found");
        return 1;
    }
    p->value(value != 0);
    return RIVE_OK;
}

extern "C" RiveStatus rive_artboard_vm_get_bool(RiveArtboard* artboard,
                                                const char* path, uint8_t* out)
{
    rive::ViewModelInstanceRuntime* vm = vm_of(artboard, path);
    if (vm == nullptr)
        return 1;
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

extern "C" RiveStatus rive_artboard_vm_fire_trigger(RiveArtboard* artboard,
                                                    const char* path)
{
    rive::ViewModelInstanceRuntime* vm = vm_of(artboard, path);
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
    if (artboard == nullptr || artboard->vmRuntime == nullptr)
    {
        shim_set_error("artboard has no view model");
        return 1;
    }
    std::vector<rive::PropertyData> props = artboard->vmRuntime->properties();
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
