/*
 * rive_shim_text.cpp — runtime text-run get/set C ABI.
 *
 * Per-feature shim TU (see rive_shim_internal.hpp + docs/feature-support.md):
 * read / write a TextValueRun's string by authored name on an artboard. The run
 * may live on the TOP-LEVEL artboard (null/empty path) or in a NESTED artboard
 * (a '/'-style path, resolved by ArtboardInstance::getTextRun). Setting a run's
 * text re-shapes it on the next advance/draw. Introspection lists top-level runs.
 */
#include "rive_shim_internal.hpp"

#include <cstring>
#include <string>

#include "rive/text/text_value_run.hpp" // TextValueRun (+ name() via Component)

namespace {

// Two-call string copy (mirrors the view-model TU): always set *out_len to the
// full length (call with cap=0 to size), copy min(cap,len) bytes, no NUL — the
// caller slices to *out_len.
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

// Resolves a TextValueRun by `name` (+ optional nested-artboard `path`) on the
// handle's artboard. `path == nullptr` or "" → the top-level artboard:
// ArtboardInstance::getTextRun only handles NON-empty paths, so top-level uses
// find<TextValueRun> directly — exactly what getTextRun does internally for the
// nested case. Returns null (no error set) on bad handle / not found; callers set
// the not-found error.
rive::TextValueRun* resolve_run(RiveArtboard* handle, const char* name, const char* path)
{
    if (handle == nullptr || handle->artboard == nullptr || name == nullptr)
        return nullptr;
    rive::ArtboardInstance* ab = handle->artboard.get();
    if (path == nullptr || path[0] == '\0')
        return ab->find<rive::TextValueRun>(std::string(name));
    return ab->getTextRun(std::string(name), std::string(path));
}

} // namespace

// Sets the text of the run named `name` (in nested artboard `path`, or the
// top-level artboard when `path` is null/empty). Re-shapes on the next advance.
extern "C" RiveStatus rive_artboard_text_set(RiveArtboard* artboard, const char* name,
                                             const char* path, const char* value)
{
    if (value == nullptr)
    {
        shim_set_error("text run value is null");
        return 1;
    }
    rive::TextValueRun* run = resolve_run(artboard, name, path);
    if (run == nullptr)
    {
        shim_set_error("text run not found");
        return 1;
    }
    run->text(std::string(value));
    return RIVE_OK;
}

// Reads the text of the run named `name` (two-call protocol: buf=null/cap=0 to
// size, then fill; bytes are NOT NUL-terminated — the caller slices to *out_len).
extern "C" RiveStatus rive_artboard_text_get(RiveArtboard* artboard, const char* name,
                                             const char* path, char* buf, size_t cap,
                                             size_t* out_len)
{
    rive::TextValueRun* run = resolve_run(artboard, name, path);
    if (run == nullptr)
    {
        shim_set_error("text run not found");
        return 1;
    }
    copy_to_caller(run->text(), buf, cap, out_len);
    return RIVE_OK;
}

// Introspection: number of TextValueRuns on the TOP-LEVEL artboard (for
// discovering names settable via rive_artboard_text_set with an empty path).
extern "C" uint32_t rive_artboard_text_run_count(RiveArtboard* artboard)
{
    if (artboard == nullptr || artboard->artboard == nullptr)
        return 0;
    return static_cast<uint32_t>(artboard->artboard->count<rive::TextValueRun>());
}

// Introspection: authored name of the i-th top-level TextValueRun (in artboard
// object order; two-call protocol). Returns nonzero if `index` is out of range.
extern "C" RiveStatus rive_artboard_text_run_name_at(RiveArtboard* artboard, uint32_t index,
                                                     char* buf, size_t cap, size_t* out_len)
{
    if (artboard == nullptr || artboard->artboard == nullptr)
    {
        shim_set_error("invalid artboard handle");
        return 1;
    }
    uint32_t i = 0;
    for (rive::Core* obj : artboard->artboard->objects())
    {
        if (obj != nullptr && obj->is<rive::TextValueRun>())
        {
            if (i == index)
            {
                copy_to_caller(obj->as<rive::TextValueRun>()->name(), buf, cap, out_len);
                return RIVE_OK;
            }
            i += 1;
        }
    }
    shim_set_error("text run index out of range");
    return 1;
}
