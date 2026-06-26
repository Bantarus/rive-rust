//! Runtime **input** for Bevy — drive a `.riv`'s joystick / keyboard / gamepad /
//! focus at runtime. Attach a [`RiveInput`] to the same entity as
//! [`RiveAnimation`](crate::RiveAnimation) and queue commands; each is applied
//! before the next advance (so the state machine / scripts / linked animations see
//! it this tick), in BOTH tiers (`floor` inline; `zero_copy` ferried to the render
//! world, like the view-model / text / rig writes).
//!
//! Two shapes (see [`rive_renderer`](rive_renderer) `input`):
//! - **Joystick** ([`set_joystick`](RiveInput::set_joystick)) drives an authored
//!   joystick component (it sticks unless an animation also keys it — re-queue each
//!   frame for continuous control, like the rig writes).
//! - **Keyboard / gamepad / focus** are an event feed routed to the focused
//!   element's listeners; they only do something when the `.riv` authors focus +
//!   key/gamepad listeners.

use bevy::prelude::*;

pub use rive_renderer::{FocusDir, GamepadAxis, GamepadButton, Key, KeyModifiers};

/// One queued input command, applied to the instance before the next advance.
#[derive(Clone, Debug)]
pub(crate) enum InputCmd {
    /// Set an authored joystick's normalized position (artboard).
    Joystick { name: String, x: f32, y: f32 },
    /// Feed a key press/release (state machine).
    Key {
        key: Key,
        modifiers: KeyModifiers,
        pressed: bool,
        repeat: bool,
    },
    /// Feed committed / IME text (state machine).
    Text(String),
    /// Feed a gamepad button analog value (state machine); pressed at `value >= 0.5`.
    GamepadButton { button: GamepadButton, value: f32 },
    /// Feed a gamepad axis value (state machine).
    GamepadAxis { axis: GamepadAxis, value: f32 },
    /// Move the primary focus in a direction (state machine).
    Focus(FocusDir),
    /// Clear the primary focus (state machine).
    ClearFocus,
}

/// Queues runtime **input** (joystick / keyboard / gamepad / focus) for a `.riv`
/// instance. Attach to the same entity as [`RiveAnimation`](crate::RiveAnimation);
/// each queued command is applied before the next advance. Honored in both tiers.
///
/// Joystick sets are a *state* (re-queue each frame for continuous control);
/// keyboard / gamepad are *events* (queue on the press/release edge). All take
/// effect on the next advance/draw. A joystick set sticks only if the active
/// animation does not *also* key it; keyboard / gamepad / focus only do something
/// when the `.riv` authors the matching focus + listeners.
#[derive(Component, Default, Debug)]
pub struct RiveInput {
    /// Pending commands, drained + applied before each advance.
    cmds: Vec<InputCmd>,
    /// `zero_copy` double-buffer: `cmds` are moved here (main world) so the
    /// read-only extract can ferry them to the render world, then cleared the
    /// following frame. Absent under `floor` (it drains `cmds` inline).
    #[cfg(feature = "zero_copy")]
    staged: Vec<InputCmd>,
}

impl RiveInput {
    /// Queues a set of the normalized position (`x`, `y`, each in `[-1, 1]`) of
    /// the authored joystick named `name`.
    pub fn set_joystick(&mut self, name: impl Into<String>, x: f32, y: f32) {
        self.cmds.push(InputCmd::Joystick {
            name: name.into(),
            x,
            y,
        });
    }

    /// Queues a key event with explicit modifiers / repeat. For the common
    /// press/release, see [`key_down`](Self::key_down) / [`key_up`](Self::key_up).
    pub fn key(&mut self, key: Key, modifiers: KeyModifiers, pressed: bool, repeat: bool) {
        self.cmds.push(InputCmd::Key {
            key,
            modifiers,
            pressed,
            repeat,
        });
    }

    /// Queues a key **press** (no modifiers, not a repeat).
    pub fn key_down(&mut self, key: Key) {
        self.key(key, KeyModifiers::NONE, true, false);
    }

    /// Queues a key **release** (no modifiers).
    pub fn key_up(&mut self, key: Key) {
        self.key(key, KeyModifiers::NONE, false, false);
    }

    /// Queues committed / IME text input (UTF-8).
    pub fn text(&mut self, text: impl Into<String>) {
        self.cmds.push(InputCmd::Text(text.into()));
    }

    /// Queues a gamepad button analog `value` in `[0, 1]` — pressed at `value >= 0.5`
    /// (pass `1.0` to press, `0.0` to release).
    pub fn gamepad_button(&mut self, button: GamepadButton, value: f32) {
        self.cmds.push(InputCmd::GamepadButton { button, value });
    }

    /// Queues a gamepad axis value (sticks `[-1, 1]`, triggers `[0, 1]`).
    pub fn gamepad_axis(&mut self, axis: GamepadAxis, value: f32) {
        self.cmds.push(InputCmd::GamepadAxis { axis, value });
    }

    /// Queues a focus move in `dir` (tab order or spatial).
    pub fn focus(&mut self, dir: FocusDir) {
        self.cmds.push(InputCmd::Focus(dir));
    }

    /// Queues a clear of the primary focus.
    pub fn clear_focus(&mut self) {
        self.cmds.push(InputCmd::ClearFocus);
    }

    /// `zero_copy`: whether there is staging work (queued or staged commands).
    #[cfg(feature = "zero_copy")]
    pub(crate) fn has_staging_work(&self) -> bool {
        !self.cmds.is_empty() || !self.staged.is_empty()
    }

    /// `zero_copy`: move this frame's queued commands into the staging buffer (or
    /// clear last frame's), so the read-only extract can ferry them.
    #[cfg(feature = "zero_copy")]
    pub(crate) fn stage_writes(&mut self) {
        if self.cmds.is_empty() {
            self.staged.clear();
        } else {
            self.staged = std::mem::take(&mut self.cmds);
        }
    }

    /// `zero_copy`: the commands staged for this frame (ferried by extract).
    #[cfg(feature = "zero_copy")]
    pub(crate) fn staged(&self) -> &[InputCmd] {
        &self.staged
    }
}

/// Applies a slice of input commands. Shared by both tiers (`floor` drains inline;
/// `zero_copy` ferries a slice to the render world). Joystick sets route to the
/// artboard; keyboard / gamepad / focus to the state machine. Per-command joystick
/// failures `warn!` and continue; the event feeds are fire-and-forget (their
/// `bool` "consumed" return is irrelevant to a queued write).
#[cfg(any(feature = "floor", feature = "zero_copy"))]
pub(crate) fn apply_input_cmds_slice(
    artboard: &rive_renderer::Artboard,
    state_machine: &mut rive_renderer::StateMachine,
    cmds: &[InputCmd],
) {
    for c in cmds {
        match c {
            InputCmd::Joystick { name, x, y } => {
                if let Err(e) = artboard.joystick_set(name, *x, *y) {
                    warn!("rive: joystick set {name:?} failed: {e}");
                }
            }
            InputCmd::Key {
                key,
                modifiers,
                pressed,
                repeat,
            } => {
                state_machine.key_input(*key, *modifiers, *pressed, *repeat);
            }
            InputCmd::Text(text) => {
                state_machine.text_input(text);
            }
            InputCmd::GamepadButton { button, value } => {
                state_machine.gamepad_button(*button, *value);
            }
            InputCmd::GamepadAxis { axis, value } => {
                state_machine.gamepad_axis(*axis, *value);
            }
            InputCmd::Focus(dir) => {
                state_machine.focus_advance(*dir);
            }
            InputCmd::ClearFocus => {
                state_machine.clear_focus();
            }
        }
    }
}

/// Drains queued input commands. Call **before** advancing so the change is solved
/// + visible this tick.
#[cfg(feature = "floor")]
pub(crate) fn apply_input_cmds(
    input: &mut RiveInput,
    artboard: &rive_renderer::Artboard,
    state_machine: &mut rive_renderer::StateMachine,
) {
    let cmds = std::mem::take(&mut input.cmds);
    apply_input_cmds_slice(artboard, state_machine, &cmds);
}
