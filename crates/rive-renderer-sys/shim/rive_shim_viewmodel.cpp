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
#include "rive/viewmodel/runtime/viewmodel_instance_color_runtime.hpp"
#include "rive/viewmodel/runtime/viewmodel_instance_string_runtime.hpp"
#include "rive/viewmodel/runtime/viewmodel_instance_enum_runtime.hpp"
#include "rive/viewmodel/runtime/viewmodel_runtime.hpp" // PropertyData

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
} // namespace

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

// ---- color (ARGB u32 <-> rive's int) ----

extern "C" RiveStatus rive_artboard_vm_set_color(RiveArtboard* artboard,
                                                 const char* path, uint32_t argb)
{
    rive::ViewModelInstanceRuntime* vm = vm_of(artboard, path);
    if (vm == nullptr)
        return 1;
    auto* p = vm->propertyColor(path);
    if (p == nullptr)
    {
        shim_set_error("view-model color property not found");
        return 1;
    }
    p->value(static_cast<int>(argb));
    return RIVE_OK;
}

extern "C" RiveStatus rive_artboard_vm_get_color(RiveArtboard* artboard,
                                                 const char* path, uint32_t* out)
{
    rive::ViewModelInstanceRuntime* vm = vm_of(artboard, path);
    if (vm == nullptr)
        return 1;
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

// ---- string (UTF-8; get via the two-call buffer protocol) ----

extern "C" RiveStatus rive_artboard_vm_set_string(RiveArtboard* artboard,
                                                  const char* path, const char* value)
{
    rive::ViewModelInstanceRuntime* vm = vm_of(artboard, path);
    if (vm == nullptr)
        return 1;
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

extern "C" RiveStatus rive_artboard_vm_get_string(RiveArtboard* artboard,
                                                  const char* path, char* buf,
                                                  size_t cap, size_t* out_len)
{
    rive::ViewModelInstanceRuntime* vm = vm_of(artboard, path);
    if (vm == nullptr)
        return 1;
    auto* p = vm->propertyString(path);
    if (p == nullptr)
    {
        shim_set_error("view-model string property not found");
        return 1;
    }
    copy_to_caller(p->value(), buf, cap, out_len);
    return RIVE_OK;
}

// ---- enum (get/set by index or name; enumerate the value labels) ----

extern "C" RiveStatus rive_artboard_vm_set_enum_index(RiveArtboard* artboard,
                                                      const char* path, uint32_t index)
{
    rive::ViewModelInstanceRuntime* vm = vm_of(artboard, path);
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
    rive::ViewModelInstanceRuntime* vm = vm_of(artboard, path);
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
    *out = p->valueIndex();
    return RIVE_OK;
}

extern "C" RiveStatus rive_artboard_vm_set_enum_name(RiveArtboard* artboard,
                                                     const char* path, const char* name)
{
    rive::ViewModelInstanceRuntime* vm = vm_of(artboard, path);
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
    rive::ViewModelInstanceRuntime* vm = vm_of(artboard, path);
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
    rive::ViewModelInstanceRuntime* vm = vm_of(artboard, path);
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
