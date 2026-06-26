/*
 * rive_shim_nested.cpp — runtime NESTED-ARTBOARD access C ABI.
 *
 * Per-feature shim TU (see rive_shim_internal.hpp + docs/feature-support.md):
 * reach INTO a child artboard mounted by a NestedArtboard component on a parent.
 *   - introspection : nested_count / nested_name_at list the NestedArtboard
 *                     component names on an artboard (top-level OR a nested child,
 *                     since these operate on handle->inst()).
 *   - resolve       : nested_named / nested_at_path return a BORROWED RiveArtboard
 *                     handle wrapping the child ArtboardInstance* (owned by the
 *                     parent's NestedArtboard). The SAME find<T>-by-name functions
 *                     (bones / text / joysticks / solo / constraints) then drive
 *                     the child — see RiveArtboard::inst() / `borrowed` in the
 *                     internal header. The child is auto-advanced by the parent, so
 *                     writes follow the usual assert-before-advance contract.
 *
 * The child is mounted into m_Instance when the parent is instanced
 * (NestedArtboard::clone), so a child resolves immediately after the file selector
 * — no advance required. The borrowed handle is freed by rive_artboard_destroy
 * (its owned `artboard` is null → .reset() no-ops; the wrapper is deleted; the
 * borrowed instance stays with its parent). It is only valid while the parent
 * artboard lives — the safe layer ties this with a lifetime.
 */
#include "rive_shim_internal.hpp"

#include <cstring>
#include <new>
#include <string>
#include <vector>

#include "rive/component.hpp"        // Component::name()
#include "rive/nested_artboard.hpp"  // NestedArtboard (artboardInstance / nestedArtboard*)

namespace {
// Two-call string copy (mirrors the rig/text/view-model TUs): always set *out_len
// to the full length (call with cap=0 to size), copy min(cap,len) bytes, no NUL —
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

// Wrap a borrowed child instance in a heap RiveArtboard (owned `artboard` left
// null, so destroy frees only the wrapper). Null + error on OOM.
RiveArtboard* make_borrowed(rive::ArtboardInstance* child)
{
    auto* h = new (std::nothrow) RiveArtboard();
    if (h == nullptr)
    {
        shim_set_error("out of memory allocating nested RiveArtboard");
        return nullptr;
    }
    h->borrowed = child;
    return h;
}
} // namespace

// Introspection: number of NestedArtboard components on this artboard (the names
// nested_named can resolve). Works on a top-level OR a nested handle.
extern "C" uint32_t rive_artboard_nested_count(RiveArtboard* artboard)
{
    rive::ArtboardInstance* ab = artboard != nullptr ? artboard->inst() : nullptr;
    if (ab == nullptr)
        return 0;
    return static_cast<uint32_t>(ab->nestedArtboards().size());
}

// Introspection: authored name of the i-th NestedArtboard component (two-call
// protocol). Returns nonzero if `index` is out of range. This name is exactly what
// nested_named accepts (Artboard::nestedArtboard matches on Component::name()).
extern "C" RiveStatus rive_artboard_nested_name_at(RiveArtboard* artboard, uint32_t index,
                                                   char* buf, size_t cap, size_t* out_len)
{
    rive::ArtboardInstance* ab = artboard != nullptr ? artboard->inst() : nullptr;
    if (ab == nullptr)
    {
        shim_set_error("invalid artboard handle");
        return 1;
    }
    const std::vector<rive::NestedArtboard*>& nested = ab->nestedArtboards();
    if (index >= nested.size())
    {
        shim_set_error("nested artboard index out of range");
        return 1;
    }
    copy_to_caller(nested[index]->name(), buf, cap, out_len);
    return RIVE_OK;
}

// Resolve a nested child by its 0-based index in nestedArtboards() (the same order
// as nested_name_at); returns a BORROWED RiveArtboard handle. Essential when the
// NestedArtboard components are unnamed (name lookup can't disambiguate). Null +
// error if out of range or the nested artboard has no mounted instance.
extern "C" RiveArtboard* rive_artboard_nested_at(RiveArtboard* artboard, uint32_t index)
{
    rive::ArtboardInstance* ab = artboard != nullptr ? artboard->inst() : nullptr;
    if (ab == nullptr)
    {
        shim_set_error("rive_artboard_nested_at: invalid handle");
        return nullptr;
    }
    const std::vector<rive::NestedArtboard*>& nested = ab->nestedArtboards();
    if (index >= nested.size())
    {
        shim_set_error("nested artboard index out of range");
        return nullptr;
    }
    rive::ArtboardInstance* child = nested[index]->artboardInstance();
    if (child == nullptr)
    {
        shim_set_error("nested artboard has no mounted instance");
        return nullptr;
    }
    return make_borrowed(child);
}

// Resolve a nested child by its NestedArtboard component name; returns a BORROWED
// RiveArtboard handle (free with rive_artboard_destroy). Null + error if no such
// nested artboard, or if it has no mounted instance.
extern "C" RiveArtboard* rive_artboard_nested_named(RiveArtboard* artboard, const char* name)
{
    rive::ArtboardInstance* ab = artboard != nullptr ? artboard->inst() : nullptr;
    if (ab == nullptr || name == nullptr)
    {
        shim_set_error("rive_artboard_nested_named: invalid handle or name");
        return nullptr;
    }
    rive::NestedArtboard* na = ab->nestedArtboard(std::string(name));
    if (na == nullptr)
    {
        shim_set_error("nested artboard not found by name");
        return nullptr;
    }
    rive::ArtboardInstance* child = na->artboardInstance();
    if (child == nullptr)
    {
        shim_set_error("nested artboard has no mounted instance");
        return nullptr;
    }
    return make_borrowed(child);
}

// Resolve a nested child by a '/'-delimited path ("child/grandchild") that descends
// through nested artboards; returns a BORROWED RiveArtboard handle. Null + error if
// the path does not resolve, or the resolved nested artboard has no instance.
extern "C" RiveArtboard* rive_artboard_nested_at_path(RiveArtboard* artboard, const char* path)
{
    rive::ArtboardInstance* ab = artboard != nullptr ? artboard->inst() : nullptr;
    if (ab == nullptr || path == nullptr)
    {
        shim_set_error("rive_artboard_nested_at_path: invalid handle or path");
        return nullptr;
    }
    rive::NestedArtboard* na = ab->nestedArtboardAtPath(std::string(path));
    if (na == nullptr)
    {
        shim_set_error("nested artboard not found at path");
        return nullptr;
    }
    rive::ArtboardInstance* child = na->artboardInstance();
    if (child == nullptr)
    {
        shim_set_error("nested artboard has no mounted instance");
        return nullptr;
    }
    return make_borrowed(child);
}
