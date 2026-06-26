//! Runtime **input control** — drive a `.riv`'s host-facing inputs:
//!
//! - **Joystick** — an authored [`Joystick`](https://rive.app/docs) component,
//!   addressed by name on an [`Artboard`] ([`joystick_set`](Artboard::joystick_set)).
//!   Set its normalized `x`/`y` in `[-1, 1]`; the artboard *applies* it during the
//!   next [`advance`](crate::StateMachine::advance) (driving the linked animations /
//!   constraints), so — like the rig writes — a set sticks only if the active
//!   animation does not *also* key it.
//! - **Keyboard / gamepad / focus** — a state-machine event feed on a
//!   [`StateMachine`], routed through the state machine's focus tree to the focused
//!   element's listeners. The focus tree is built automatically when the state
//!   machine is created, so these need no setup — but they only *do* something when
//!   the `.riv` authors focus + key/gamepad listeners (otherwise they are a no-op
//!   and report "not consumed").
//!
//! Mirrors the Rive runtime input API. The methods extend [`Artboard`] and
//! [`StateMachine`] (both defined in `scene.rs`), alongside the `vm_*` / `text_*` /
//! `bone_*` accessors and `pointer_*`.

use std::ffi::CString;
use std::os::raw::c_char;

use crate::{last_error, sys, Artboard, Error, Result, StateMachine};

/// A keyboard key, matching `rive::Key` (the GLFW key layout). Map your engine's
/// key codes to these before calling [`StateMachine::key_input`]. The discriminant
/// is the raw `rive::Key` value, so common keys line up with ASCII
/// (`A` = 65, [`Space`](Self::Space) = 32).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
#[rustfmt::skip]
#[allow(missing_docs)] // names are self-describing; one doc per key would be noise
pub enum Key {
    Space = 32,
    Apostrophe = 39,
    Comma = 44,
    Minus = 45,
    Period = 46,
    Slash = 47,
    Key0 = 48, Key1 = 49, Key2 = 50, Key3 = 51, Key4 = 52,
    Key5 = 53, Key6 = 54, Key7 = 55, Key8 = 56, Key9 = 57,
    Semicolon = 59,
    Equal = 61,
    A = 65, B = 66, C = 67, D = 68, E = 69, F = 70, G = 71, H = 72,
    I = 73, J = 74, K = 75, L = 76, M = 77, N = 78, O = 79, P = 80,
    Q = 81, R = 82, S = 83, T = 84, U = 85, V = 86, W = 87, X = 88,
    Y = 89, Z = 90,
    LeftBracket = 91, Backslash = 92, RightBracket = 93, GraveAccent = 96,
    World1 = 161, World2 = 162,
    Escape = 256, Enter = 257, Tab = 258, Backspace = 259, Insert = 260,
    Delete = 261, Right = 262, Left = 263, Down = 264, Up = 265,
    PageUp = 266, PageDown = 267, Home = 268, End = 269,
    CapsLock = 280, ScrollLock = 281, NumLock = 282, PrintScreen = 283, Pause = 284,
    F1 = 290, F2 = 291, F3 = 292, F4 = 293, F5 = 294, F6 = 295, F7 = 296,
    F8 = 297, F9 = 298, F10 = 299, F11 = 300, F12 = 301, F13 = 302, F14 = 303,
    F15 = 304, F16 = 305, F17 = 306, F18 = 307, F19 = 308, F20 = 309, F21 = 310,
    F22 = 311, F23 = 312, F24 = 313, F25 = 314,
    Kp0 = 320, Kp1 = 321, Kp2 = 322, Kp3 = 323, Kp4 = 324, Kp5 = 325,
    Kp6 = 326, Kp7 = 327, Kp8 = 328, Kp9 = 329, KpDecimal = 330, KpDivide = 331,
    KpMultiply = 332, KpSubtract = 333, KpAdd = 334, KpEnter = 335, KpEqual = 336,
    LeftShift = 340, LeftControl = 341, LeftAlt = 342, LeftSuper = 343,
    RightShift = 344, RightControl = 345, RightAlt = 346, RightSuper = 347,
    Menu = 348,
}

/// Held modifier keys for [`StateMachine::key_input`] (a bitmask matching
/// `rive::KeyModifiers`). Combine with `|`:
/// `KeyModifiers::CTRL | KeyModifiers::SHIFT`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyModifiers(u8);

impl KeyModifiers {
    /// No modifiers held.
    pub const NONE: Self = Self(0);
    /// Either shift key.
    pub const SHIFT: Self = Self(1 << 0);
    /// Either control key.
    pub const CTRL: Self = Self(1 << 1);
    /// Either alt / option key.
    pub const ALT: Self = Self(1 << 2);
    /// Either meta / super / command / windows key.
    pub const META: Self = Self(1 << 3);

    /// The raw bitmask (as passed to the runtime).
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Whether every modifier in `other` is also held in `self`.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl core::ops::BitOr for KeyModifiers {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// A W3C Standard Gamepad button for [`StateMachine::gamepad_button`]. The
/// discriminant is the W3C button index ([remapping](https://w3c.github.io/gamepad/#remapping)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
#[allow(missing_docs)] // W3C standard layout; names match the spec
pub enum GamepadButton {
    South = 0,
    East = 1,
    West = 2,
    North = 3,
    LeftShoulder = 4,
    RightShoulder = 5,
    LeftTrigger = 6,
    RightTrigger = 7,
    Back = 8,
    Forward = 9,
    LeftStick = 10,
    RightStick = 11,
    DpadUp = 12,
    DpadDown = 13,
    DpadLeft = 14,
    DpadRight = 15,
    Start = 16,
}

/// A W3C Standard Gamepad analog axis for [`StateMachine::gamepad_axis`]. Sticks
/// range `[-1, 1]`; triggers range `[0, 1]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
#[allow(missing_docs)] // W3C standard layout
pub enum GamepadAxis {
    LeftX = 0,
    LeftY = 1,
    RightX = 2,
    RightY = 3,
    LeftTrigger = 4,
    RightTrigger = 5,
}

/// A focus-navigation direction for [`StateMachine::focus_advance`] — tab order
/// ([`Next`](Self::Next)/[`Prev`](Self::Prev)) or spatial (the four directions,
/// for arrow-key / d-pad navigation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum FocusDir {
    /// Next element in tab order.
    Next = 0,
    /// Previous element in tab order.
    Prev = 1,
    /// Nearest focusable to the left.
    Left = 2,
    /// Nearest focusable to the right.
    Right = 3,
    /// Nearest focusable above.
    Up = 4,
    /// Nearest focusable below.
    Down = 5,
}

/// A snapshot of a state machine's focus state, from [`StateMachine::focus_state`]
/// — for deciding whether to show a soft keyboard / IME.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FocusState {
    /// Whether any element currently holds focus.
    pub has_focus: bool,
    /// Whether the focused element accepts keyboard input (a text input, or a
    /// focusable with key/text listeners). Always `false` when nothing is focused.
    pub expects_keyboard: bool,
}

/// Maps an interior-NUL failure on an input name to [`Error::Input`].
fn input_cstring(s: &str, what: &str) -> Result<CString> {
    CString::new(s).map_err(|_| Error::Input(format!("{what} contained an interior NUL byte")))
}

/// `RIVE_OK` → `Ok(())`, otherwise the shim's last error as [`Error::Input`].
fn input_status(st: sys::RiveStatus) -> Result<()> {
    if st == sys::RIVE_OK {
        Ok(())
    } else {
        Err(Error::Input(last_error()))
    }
}

/// Runs the shim's two-call string protocol (size with a null buffer, then fill).
fn read_string_via<F>(call: F) -> Result<String>
where
    F: Fn(*mut c_char, usize, *mut usize) -> sys::RiveStatus,
{
    let mut len = 0_usize;
    input_status(call(std::ptr::null_mut(), 0, &mut len))?;
    let mut buf = vec![0_u8; len];
    let mut written = 0_usize;
    input_status(call(buf.as_mut_ptr().cast::<c_char>(), buf.len(), &mut written))?;
    Ok(String::from_utf8_lossy(&buf[..written.min(buf.len())]).into_owned())
}

impl Artboard {
    /// Sets the normalized position (`x`, `y`, each in `[-1, 1]`) of the joystick
    /// named `name`. Applied on the next advance.
    ///
    /// # Errors
    ///
    /// [`Error::Input`] if no joystick with that name exists, or `name` contained
    /// an interior NUL byte.
    pub fn joystick_set(&self, name: &str, x: f32, y: f32) -> Result<()> {
        let name_c = input_cstring(name, "joystick name")?;
        // SAFETY: live artboard handle; `name_c` is a valid C string.
        let st = unsafe { sys::rive_artboard_joystick_set(self.inner.ptr, name_c.as_ptr(), x, y) };
        input_status(st)
    }

    /// Reads the current `(x, y)` position of the joystick named `name`.
    ///
    /// # Errors
    ///
    /// [`Error::Input`] if no joystick with that name exists, or `name` contained
    /// an interior NUL byte.
    pub fn joystick_get(&self, name: &str) -> Result<(f32, f32)> {
        let name_c = input_cstring(name, "joystick name")?;
        let mut x = 0.0_f32;
        let mut y = 0.0_f32;
        // SAFETY: live artboard handle; `name_c` valid; `x`/`y` are live f32s.
        let st = unsafe {
            sys::rive_artboard_joystick_get(self.inner.ptr, name_c.as_ptr(), &mut x, &mut y)
        };
        input_status(st).map(|()| (x, y))
    }

    /// The authored names of all joysticks on the artboard — for discovering what
    /// [`joystick_set`](Self::joystick_set) can address.
    pub fn joystick_names(&self) -> Vec<String> {
        // SAFETY: `self.inner.ptr` is a live artboard handle.
        let count = unsafe { sys::rive_artboard_joystick_count(self.inner.ptr) };
        (0..count)
            .map(|i| {
                // SAFETY: live handle; `i` < count; the shim's two-call protocol.
                read_string_via(|buf, cap, out_len| unsafe {
                    sys::rive_artboard_joystick_name_at(self.inner.ptr, i, buf, cap, out_len)
                })
                .unwrap_or_default()
            })
            .collect()
    }
}

impl StateMachine {
    /// Feeds a key press/release to the state machine's focused element. Returns
    /// `true` if a listener consumed it.
    ///
    /// No-op (returns `false`) when this scene is a linear animation, or when the
    /// `.riv` authors no focus / keyboard listeners. Feed key events **before**
    /// [`advance`](Self::advance) so listeners react on the same tick.
    pub fn key_input(
        &mut self,
        key: Key,
        modifiers: KeyModifiers,
        pressed: bool,
        repeat: bool,
    ) -> bool {
        // SAFETY: `self.ptr` is a live state-machine handle.
        unsafe {
            sys::rive_state_machine_key_input(
                self.ptr,
                key as u16,
                modifiers.bits(),
                pressed as u8,
                repeat as u8,
            ) != 0
        }
    }

    /// Feeds committed / IME text (UTF-8) to the focused element. Returns `true`
    /// if a listener consumed it (empty `text` is an effective no-op). See
    /// [`key_input`](Self::key_input) for caveats.
    pub fn text_input(&mut self, text: &str) -> bool {
        // SAFETY: `self.ptr` is a live handle; `text` is valid UTF-8 of `len` bytes
        // (the shim copies it into a std::string and does not retain the pointer).
        unsafe {
            sys::rive_state_machine_text_input(self.ptr, text.as_ptr().cast::<c_char>(), text.len())
                != 0
        }
    }

    /// Feeds a gamepad `button` analog `value` in `[0, 1]` to the focused element —
    /// the button reads as **pressed at `value >= 0.5`** (rive's listener threshold),
    /// so pass `1.0` to press and `0.0` to release (analog triggers pass their
    /// pressure). The held state accumulates across calls, so a script reading the
    /// full pad state sees every held button. Returns `true` if a listener consumed
    /// it. See [`key_input`](Self::key_input) for caveats.
    pub fn gamepad_button(&mut self, button: GamepadButton, value: f32) -> bool {
        // SAFETY: `self.ptr` is a live state-machine handle.
        unsafe { sys::rive_state_machine_gamepad_button(self.ptr, button as u8, value) != 0 }
    }

    /// Feeds a gamepad `axis` value (sticks `[-1, 1]`, triggers `[0, 1]`) to the
    /// focused element. Returns `true` if a listener consumed it. See
    /// [`key_input`](Self::key_input) for caveats.
    pub fn gamepad_axis(&mut self, axis: GamepadAxis, value: f32) -> bool {
        // SAFETY: `self.ptr` is a live state-machine handle.
        unsafe { sys::rive_state_machine_gamepad_axis(self.ptr, axis as u8, value) != 0 }
    }

    /// Moves the primary focus in `dir` (tab order or spatially). Returns `true`
    /// if focus moved. No-op (returns `false`) on a linear-animation scene or a
    /// `.riv` with no focusable elements.
    pub fn focus_advance(&mut self, dir: FocusDir) -> bool {
        // SAFETY: `self.ptr` is a live state-machine handle.
        unsafe { sys::rive_state_machine_focus_advance(self.ptr, dir as u32) != 0 }
    }

    /// Clears the primary focus (nothing focused). No-op on a linear-animation scene.
    pub fn clear_focus(&mut self) {
        // SAFETY: `self.ptr` is a live state-machine handle.
        unsafe { sys::rive_state_machine_clear_focus(self.ptr) }
    }

    /// Polls the current [`FocusState`] (what holds focus, whether it wants
    /// keyboard input). Returns the default (nothing focused) on a linear-animation
    /// scene.
    pub fn focus_state(&self) -> FocusState {
        let mut has_focus = 0_u8;
        let mut expects_keyboard = 0_u8;
        // SAFETY: `self.ptr` is a live handle; both out-params are live u8s.
        unsafe {
            sys::rive_state_machine_focus_state(self.ptr, &mut has_focus, &mut expects_keyboard);
        }
        FocusState {
            has_focus: has_focus != 0,
            expects_keyboard: expects_keyboard != 0,
        }
    }
}
