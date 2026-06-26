/*
 * rive_shim_rig.cpp — runtime RIG control C ABI: bones / constraints / solo.
 *
 * Per-feature shim TU (see rive_shim_internal.hpp + docs/feature-support.md):
 * drive a rig at runtime by AUTHORED component name, via ArtboardInstance::find<T>:
 *   - bones       : set/get a bone's transform (rotation/scaleX/scaleY/length on
 *                   any Bone; x/y on a RootBone only).
 *   - constraints : set/get a Constraint's strength (the knob every constraint has).
 *   - solo        : select the active child of a Solo (exclusive visibility), by
 *                   name or index; read it back.
 * Like text / view-model writes, a set is asserted on the artboard and takes
 * effect on the next advance/draw (advance solves on top — a written value sticks
 * only if the active animation does not ALSO key that property). Introspection
 * (rig_count / rig_name_at) lists components of a kind so a game can discover the
 * settable names in an opaque .riv.
 */
#include "rive_shim_internal.hpp"

#include <cstring>
#include <string>

#include "rive/bones/bone.hpp"               // Bone (+ length via BoneBase, rotation/scale via TransformComponent)
#include "rive/bones/root_bone.hpp"          // RootBone (x/y setters)
#include "rive/component.hpp"                 // Component::name()
#include "rive/constraints/constraint.hpp"   // Constraint (strength via ConstraintBase)
#include "rive/solo.hpp"                      // Solo

// Bone property selector — mirrors RIVE_BONE_* in rive_shim.h.
namespace {
enum BoneProp {
    BONE_ROTATION = 0,
    BONE_SCALE_X = 1,
    BONE_SCALE_Y = 2,
    BONE_LENGTH = 3,
    BONE_X = 4,
    BONE_Y = 5,
};

// Rig component kind for introspection — mirrors RIVE_RIG_* in rive_shim.h.
enum RigKind {
    RIG_BONE = 0,
    RIG_CONSTRAINT = 1,
    RIG_SOLO = 2,
};

// Two-call string copy (mirrors the text/view-model TUs): always set *out_len to
// the full length (call with cap=0 to size), copy min(cap,len) bytes, no NUL —
// the caller slices to *out_len.
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

rive::ArtboardInstance* artboard_of(RiveArtboard* handle)
{
    // inst() resolves to the borrowed nested child if this is a nested handle,
    // else the owned instance — so these find<T> functions drive a child too.
    return handle != nullptr ? handle->inst() : nullptr;
}

// True if `obj` is a component of `kind` (Bone includes RootBone; Constraint
// includes every concrete constraint — verified via isTypeOf inheritance).
bool obj_is_kind(rive::Core* obj, uint32_t kind)
{
    switch (kind)
    {
        case RIG_BONE:
            return obj->is<rive::Bone>();
        case RIG_CONSTRAINT:
            return obj->is<rive::Constraint>();
        case RIG_SOLO:
            return obj->is<rive::Solo>();
        default:
            return false;
    }
}

} // namespace

// --- Bones ------------------------------------------------------------------

// Sets bone property `prop` (RIVE_BONE_*) on the bone named `name`. ROTATION/
// SCALE_X/SCALE_Y/LENGTH apply to any bone; X/Y are settable on ROOT bones only.
extern "C" RiveStatus rive_artboard_bone_set(RiveArtboard* artboard, const char* name,
                                             uint32_t prop, float value)
{
    rive::ArtboardInstance* ab = artboard_of(artboard);
    if (ab == nullptr || name == nullptr)
    {
        shim_set_error("invalid artboard handle");
        return 1;
    }
    if (prop == BONE_X || prop == BONE_Y)
    {
        rive::RootBone* rb = ab->find<rive::RootBone>(std::string(name));
        if (rb == nullptr)
        {
            shim_set_error("root bone not found (x/y settable on root bones only)");
            return 1;
        }
        if (prop == BONE_X)
            rb->x(value);
        else
            rb->y(value);
        return RIVE_OK;
    }
    rive::Bone* bone = ab->find<rive::Bone>(std::string(name));
    if (bone == nullptr)
    {
        shim_set_error("bone not found");
        return 1;
    }
    switch (prop)
    {
        case BONE_ROTATION:
            bone->rotation(value);
            return RIVE_OK;
        case BONE_SCALE_X:
            bone->scaleX(value);
            return RIVE_OK;
        case BONE_SCALE_Y:
            bone->scaleY(value);
            return RIVE_OK;
        case BONE_LENGTH:
            bone->length(value);
            return RIVE_OK;
        default:
            shim_set_error("unknown bone property");
            return 1;
    }
}

// Reads bone property `prop` (RIVE_BONE_*) of the bone named `name` into *out.
extern "C" RiveStatus rive_artboard_bone_get(RiveArtboard* artboard, const char* name,
                                             uint32_t prop, float* out)
{
    rive::ArtboardInstance* ab = artboard_of(artboard);
    if (ab == nullptr || name == nullptr || out == nullptr)
    {
        shim_set_error("invalid artboard handle");
        return 1;
    }
    if (prop == BONE_X || prop == BONE_Y)
    {
        rive::RootBone* rb = ab->find<rive::RootBone>(std::string(name));
        if (rb == nullptr)
        {
            shim_set_error("root bone not found (x/y readable on root bones only)");
            return 1;
        }
        *out = (prop == BONE_X) ? rb->x() : rb->y();
        return RIVE_OK;
    }
    rive::Bone* bone = ab->find<rive::Bone>(std::string(name));
    if (bone == nullptr)
    {
        shim_set_error("bone not found");
        return 1;
    }
    switch (prop)
    {
        case BONE_ROTATION:
            *out = bone->rotation();
            return RIVE_OK;
        case BONE_SCALE_X:
            *out = bone->scaleX();
            return RIVE_OK;
        case BONE_SCALE_Y:
            *out = bone->scaleY();
            return RIVE_OK;
        case BONE_LENGTH:
            *out = bone->length();
            return RIVE_OK;
        default:
            shim_set_error("unknown bone property");
            return 1;
    }
}

// --- Constraints ------------------------------------------------------------

// Sets the strength (typically [0,1]) of the constraint named `name`.
extern "C" RiveStatus rive_artboard_constraint_set_strength(RiveArtboard* artboard,
                                                            const char* name, float value)
{
    rive::ArtboardInstance* ab = artboard_of(artboard);
    if (ab == nullptr || name == nullptr)
    {
        shim_set_error("invalid artboard handle");
        return 1;
    }
    rive::Constraint* c = ab->find<rive::Constraint>(std::string(name));
    if (c == nullptr)
    {
        shim_set_error("constraint not found");
        return 1;
    }
    c->strength(value);
    return RIVE_OK;
}

// Reads the strength of the constraint named `name` into *out.
extern "C" RiveStatus rive_artboard_constraint_get_strength(RiveArtboard* artboard,
                                                            const char* name, float* out)
{
    rive::ArtboardInstance* ab = artboard_of(artboard);
    if (ab == nullptr || name == nullptr || out == nullptr)
    {
        shim_set_error("invalid artboard handle");
        return 1;
    }
    rive::Constraint* c = ab->find<rive::Constraint>(std::string(name));
    if (c == nullptr)
    {
        shim_set_error("constraint not found");
        return 1;
    }
    *out = c->strength();
    return RIVE_OK;
}

// --- Solo -------------------------------------------------------------------

// Selects the active child (by authored name) of the solo named `name`. Confirms
// the child exists (updateByName silently no-ops on a miss) → error otherwise.
extern "C" RiveStatus rive_artboard_solo_set_active_name(RiveArtboard* artboard,
                                                         const char* name, const char* child)
{
    rive::ArtboardInstance* ab = artboard_of(artboard);
    if (ab == nullptr || name == nullptr || child == nullptr)
    {
        shim_set_error("invalid artboard handle");
        return 1;
    }
    rive::Solo* solo = ab->find<rive::Solo>(std::string(name));
    if (solo == nullptr)
    {
        shim_set_error("solo not found");
        return 1;
    }
    solo->updateByName(std::string(child));
    if (solo->getActiveChildName() != std::string(child))
    {
        shim_set_error("solo child not found");
        return 1;
    }
    return RIVE_OK;
}

// Selects the active child (by 0-based index) of the solo named `name`. Confirms
// the index was in range (updateByIndex no-ops when out of range) → error otherwise.
extern "C" RiveStatus rive_artboard_solo_set_active_index(RiveArtboard* artboard,
                                                          const char* name, uint32_t index)
{
    rive::ArtboardInstance* ab = artboard_of(artboard);
    if (ab == nullptr || name == nullptr)
    {
        shim_set_error("invalid artboard handle");
        return 1;
    }
    rive::Solo* solo = ab->find<rive::Solo>(std::string(name));
    if (solo == nullptr)
    {
        shim_set_error("solo not found");
        return 1;
    }
    solo->updateByIndex(static_cast<size_t>(index));
    if (solo->getActiveChildIndex() != static_cast<int>(index))
    {
        shim_set_error("solo child index out of range");
        return 1;
    }
    return RIVE_OK;
}

// Reads the active child's authored name of the solo named `name` (two-call
// protocol). An empty result means no child is currently active.
extern "C" RiveStatus rive_artboard_solo_get_active_name(RiveArtboard* artboard,
                                                         const char* name, char* buf,
                                                         size_t cap, size_t* out_len)
{
    rive::ArtboardInstance* ab = artboard_of(artboard);
    if (ab == nullptr || name == nullptr)
    {
        shim_set_error("invalid artboard handle");
        return 1;
    }
    rive::Solo* solo = ab->find<rive::Solo>(std::string(name));
    if (solo == nullptr)
    {
        shim_set_error("solo not found");
        return 1;
    }
    copy_to_caller(solo->getActiveChildName(), buf, cap, out_len);
    return RIVE_OK;
}

// The active child's 0-based index of the solo named `name`, or -1 if none is
// active / the solo (or handle) is invalid.
extern "C" int32_t rive_artboard_solo_get_active_index(RiveArtboard* artboard, const char* name)
{
    rive::ArtboardInstance* ab = artboard_of(artboard);
    if (ab == nullptr || name == nullptr)
        return -1;
    rive::Solo* solo = ab->find<rive::Solo>(std::string(name));
    if (solo == nullptr)
        return -1;
    return solo->getActiveChildIndex();
}

// --- Introspection (discover settable component names) ----------------------

// Number of rig components of `kind` (RIVE_RIG_*) on the artboard.
extern "C" uint32_t rive_artboard_rig_count(RiveArtboard* artboard, uint32_t kind)
{
    rive::ArtboardInstance* ab = artboard_of(artboard);
    if (ab == nullptr)
        return 0;
    switch (kind)
    {
        case RIG_BONE:
            return static_cast<uint32_t>(ab->count<rive::Bone>());
        case RIG_CONSTRAINT:
            return static_cast<uint32_t>(ab->count<rive::Constraint>());
        case RIG_SOLO:
            return static_cast<uint32_t>(ab->count<rive::Solo>());
        default:
            return 0;
    }
}

// Authored name of the i-th rig component of `kind` (in artboard object order;
// two-call protocol). Returns nonzero if `index` is out of range / kind unknown.
extern "C" RiveStatus rive_artboard_rig_name_at(RiveArtboard* artboard, uint32_t kind,
                                                uint32_t index, char* buf, size_t cap,
                                                size_t* out_len)
{
    rive::ArtboardInstance* ab = artboard_of(artboard);
    if (ab == nullptr)
    {
        shim_set_error("invalid artboard handle");
        return 1;
    }
    uint32_t i = 0;
    for (rive::Core* obj : ab->objects())
    {
        if (obj != nullptr && obj_is_kind(obj, kind))
        {
            if (i == index)
            {
                copy_to_caller(obj->as<rive::Component>()->name(), buf, cap, out_len);
                return RIVE_OK;
            }
            i += 1;
        }
    }
    shim_set_error("rig component index out of range");
    return 1;
}
