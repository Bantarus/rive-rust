/*
 * rive_shim_input.cpp — runtime INPUT control C ABI: joystick / keyboard /
 * gamepad / focus.
 *
 * Per-feature shim TU (see rive_shim_internal.hpp + docs/feature-support.md). Two
 * shapes of host-driven input, mirroring the two native rive APIs:
 *
 *   - joystick : an AUTHORED component (like a bone), addressed by name via
 *                ArtboardInstance::find<Joystick>. Set its normalized x/y [-1..1];
 *                the artboard APPLIES it during advance (drives linked animations /
 *                constraints), so — like the rig writes — a set sticks unless an
 *                animation also keys it. Operates on a RiveArtboard handle.
 *
 *   - keyboard / gamepad / focus : a state-machine EVENT feed, routed through the
 *                StateMachineInstance's FocusManager to the focused FocusData's
 *                listeners. The focus tree is built automatically when the SM
 *                instance is created, so these work with no extra setup — but they
 *                only DO anything when the .riv authors FocusData + key/gamepad
 *                listeners (no-op / consumed=false otherwise). Operate on a
 *                RiveStateMachine handle (need StateMachineInstance, not the base
 *                Scene — hence the typed `smInstance` captured at construction).
 */
#include "rive_shim_internal.hpp"

#include <cstring>
#include <string>
#include <vector>

#include "rive/component.hpp"                          // Component::name()
#include "rive/joystick.hpp"                           // Joystick (x/y, find<Joystick>)
#include "rive/animation/state_machine_instance.hpp"   // StateMachineInstance (focus/keyboard/gamepad)
#include "rive/animation/listener_invocation.hpp"      // ListenerInvocation, GamepadEventInvocation
#include "rive/input/focus_manager.hpp"                // FocusManager
#include "rive/input/focusable.hpp"                    // Key, KeyModifiers
#include "rive/input/standard_gamepad.hpp"             // StandardGamepadButton / Axis

namespace {

// Focus navigation direction — mirrors RIVE_FOCUS_* in rive_shim.h.
enum FocusDir {
    FOCUS_NEXT = 0,
    FOCUS_PREV = 1,
    FOCUS_LEFT = 2,
    FOCUS_RIGHT = 3,
    FOCUS_UP = 4,
    FOCUS_DOWN = 5,
};

// Two-call string copy (mirrors the rig / text TUs): always set *out_len to the
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

rive::ArtboardInstance* artboard_of(RiveArtboard* handle)
{
    // inst() resolves to the borrowed nested child if this is a nested handle,
    // else the owned instance — so these find<T> functions drive a child too.
    return handle != nullptr ? handle->inst() : nullptr;
}

// The StateMachineInstance behind a handle, or null if the handle is null OR the
// scene is the LinearAnimationInstance fallback (which has no focus manager).
rive::StateMachineInstance* sm_of(RiveStateMachine* handle)
{
    if (handle == nullptr)
        return nullptr;
    return handle->smInstance;
}

// Grow `v` (zero-filling) so index `i` is addressable.
void ensure_index(std::vector<float>& v, uint8_t i)
{
    if (v.size() <= static_cast<size_t>(i))
        v.resize(static_cast<size_t>(i) + 1, 0.0f);
}

// Dispatch a gamepad invocation the way rive does internally (state_machine_instance
// gamepad path): bubble through the focus tree, then broadcast to any scripted
// drawables that weren't already reached. Returns 1 if the focus tree consumed it.
uint8_t dispatch_gamepad(rive::StateMachineInstance* smi, const rive::ListenerInvocation& inv)
{
    rive::ScriptedDrawable* dispatched = nullptr;
    const bool consumed = smi->focusManager()->gamepadDispatch(inv, &dispatched);
    smi->broadcastGamepadToScriptedDrawables(inv, dispatched);
    return consumed ? 1 : 0;
}

} // namespace

// === Joystick (authored component, RiveArtboard handle) =====================

extern "C" RiveStatus rive_artboard_joystick_set(RiveArtboard* artboard, const char* name,
                                                 float x, float y)
{
    rive::ArtboardInstance* ab = artboard_of(artboard);
    if (ab == nullptr || name == nullptr)
    {
        shim_set_error("invalid artboard handle");
        return 1;
    }
    rive::Joystick* j = ab->find<rive::Joystick>(std::string(name));
    if (j == nullptr)
    {
        shim_set_error("joystick not found");
        return 1;
    }
    j->x(x);
    j->y(y);
    return RIVE_OK;
}

extern "C" RiveStatus rive_artboard_joystick_get(RiveArtboard* artboard, const char* name,
                                                 float* out_x, float* out_y)
{
    rive::ArtboardInstance* ab = artboard_of(artboard);
    if (ab == nullptr || name == nullptr)
    {
        shim_set_error("invalid artboard handle");
        return 1;
    }
    rive::Joystick* j = ab->find<rive::Joystick>(std::string(name));
    if (j == nullptr)
    {
        shim_set_error("joystick not found");
        return 1;
    }
    if (out_x != nullptr)
        *out_x = j->x();
    if (out_y != nullptr)
        *out_y = j->y();
    return RIVE_OK;
}

extern "C" uint32_t rive_artboard_joystick_count(RiveArtboard* artboard)
{
    rive::ArtboardInstance* ab = artboard_of(artboard);
    if (ab == nullptr)
        return 0;
    uint32_t n = 0;
    for (rive::Core* obj : ab->objects())
        if (obj != nullptr && obj->is<rive::Joystick>())
            n += 1;
    return n;
}

extern "C" RiveStatus rive_artboard_joystick_name_at(RiveArtboard* artboard, uint32_t index,
                                                     char* buf, size_t cap, size_t* out_len)
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
        if (obj != nullptr && obj->is<rive::Joystick>())
        {
            if (i == index)
            {
                copy_to_caller(obj->as<rive::Component>()->name(), buf, cap, out_len);
                return RIVE_OK;
            }
            i += 1;
        }
    }
    shim_set_error("joystick index out of range");
    return 1;
}

// === Keyboard (state-machine event feed, RiveStateMachine handle) ===========

extern "C" uint8_t rive_state_machine_key_input(RiveStateMachine* sm, uint16_t key,
                                                uint8_t modifiers, uint8_t is_pressed,
                                                uint8_t is_repeat)
{
    rive::StateMachineInstance* smi = sm_of(sm);
    if (smi == nullptr)
        return 0;
    return smi->focusManager()->keyInput(static_cast<rive::Key>(key),
                                         static_cast<rive::KeyModifiers>(modifiers),
                                         is_pressed != 0,
                                         is_repeat != 0)
               ? 1
               : 0;
}

extern "C" uint8_t rive_state_machine_text_input(RiveStateMachine* sm, const char* utf8,
                                                 size_t len)
{
    rive::StateMachineInstance* smi = sm_of(sm);
    if (smi == nullptr)
        return 0;
    std::string text = (utf8 != nullptr && len > 0) ? std::string(utf8, len) : std::string();
    return smi->focusManager()->textInput(text) ? 1 : 0;
}

// === Gamepad (W3C-standard button / axis, RiveStateMachine handle) ==========

extern "C" uint8_t rive_state_machine_gamepad_button(RiveStateMachine* sm, uint8_t button,
                                                     float value)
{
    rive::StateMachineInstance* smi = sm_of(sm);
    if (smi == nullptr)
        return 0;
    if (button > 63)
    {
        shim_set_error("gamepad button index out of range (0..63)");
        return 0;
    }
    // Fold the change into the cumulative pad state so fullState stays faithful.
    // rive's gamepad listener derives the button down/up phase from `value >= 0.5f`
    // (listener_input_type_gamepad.cpp), so the mask bit tracks the SAME threshold —
    // a `value`-driven press is the single source of truth (no separate pressed flag
    // that rive's listener would ignore), keeping buttonMask coherent with buttonValues.
    rive::GamepadSnapshot& pad = sm->gamepad;
    const uint64_t bit = uint64_t(1) << button;
    if (value >= 0.5f)
        pad.buttonMask |= bit;
    else
        pad.buttonMask &= ~bit;
    ensure_index(pad.buttonValues, button);
    pad.buttonValues[button] = value;

    rive::GamepadEventInvocation ev;
    ev.fullState = pad;
    ev.change.kind = rive::GamepadInputChangeKind::button;
    ev.change.index = button;
    ev.change.value = value;
    ev.hasStandardButtonIntent =
        pad.mapping == rive::GamepadMappingKind::standard &&
        button <= static_cast<uint8_t>(rive::StandardGamepadButton::start);
    if (ev.hasStandardButtonIntent)
        ev.standardButton = static_cast<rive::StandardGamepadButton>(button);
    return dispatch_gamepad(smi, rive::ListenerInvocation::gamepadEvent(ev));
}

extern "C" uint8_t rive_state_machine_gamepad_axis(RiveStateMachine* sm, uint8_t axis,
                                                   float value)
{
    rive::StateMachineInstance* smi = sm_of(sm);
    if (smi == nullptr)
        return 0;
    if (axis > 63)
    {
        shim_set_error("gamepad axis index out of range (0..63)");
        return 0;
    }
    rive::GamepadSnapshot& pad = sm->gamepad;
    ensure_index(pad.axes, axis);
    pad.axes[axis] = value;

    rive::GamepadEventInvocation ev;
    ev.fullState = pad;
    ev.change.kind = rive::GamepadInputChangeKind::axis;
    ev.change.index = axis;
    ev.change.value = value;
    ev.hasStandardAxisIntent =
        pad.mapping == rive::GamepadMappingKind::standard &&
        axis <= static_cast<uint8_t>(rive::StandardGamepadAxis::rightTrigger);
    if (ev.hasStandardAxisIntent)
        ev.standardAxis = static_cast<rive::StandardGamepadAxis>(axis);
    return dispatch_gamepad(smi, rive::ListenerInvocation::gamepadEvent(ev));
}

// === Focus (navigation + state, RiveStateMachine handle) ====================

extern "C" uint8_t rive_state_machine_focus_advance(RiveStateMachine* sm, uint32_t dir)
{
    rive::StateMachineInstance* smi = sm_of(sm);
    if (smi == nullptr)
        return 0;
    rive::FocusManager* fm = smi->focusManager();
    switch (dir)
    {
        case FOCUS_NEXT:
            return fm->focusNext() ? 1 : 0;
        case FOCUS_PREV:
            return fm->focusPrevious() ? 1 : 0;
        case FOCUS_LEFT:
            return fm->focusLeft() ? 1 : 0;
        case FOCUS_RIGHT:
            return fm->focusRight() ? 1 : 0;
        case FOCUS_UP:
            return fm->focusUp() ? 1 : 0;
        case FOCUS_DOWN:
            return fm->focusDown() ? 1 : 0;
        default:
            shim_set_error("unknown focus direction");
            return 0;
    }
}

extern "C" void rive_state_machine_clear_focus(RiveStateMachine* sm)
{
    rive::StateMachineInstance* smi = sm_of(sm);
    if (smi == nullptr)
        return;
    smi->focusManager()->clearFocus();
}

extern "C" void rive_state_machine_focus_state(RiveStateMachine* sm, uint8_t* out_has_focus,
                                               uint8_t* out_expects_keyboard)
{
    rive::StateMachineInstance* smi = sm_of(sm);
    bool has_focus = false;
    bool expects_keyboard = false;
    if (smi != nullptr)
    {
        const rive::StateMachineInstance::FocusState st = smi->focusState();
        has_focus = st.hasFocus;
        expects_keyboard = st.expectsKeyboardInput;
    }
    if (out_has_focus != nullptr)
        *out_has_focus = has_focus ? 1 : 0;
    if (out_expects_keyboard != nullptr)
        *out_expects_keyboard = expects_keyboard ? 1 : 0;
}
