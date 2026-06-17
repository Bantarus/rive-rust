//! Audio engine lifecycle + master volume.
//!
//! This crate is built with `--with_rive_audio=system`, so rive owns a miniaudio
//! device and plays a `.riv`'s audio events / embedded audio straight to the OS
//! output **automatically** during [`StateMachine::advance`](crate::StateMachine::advance)
//! — there is no per-sound API to call. The engine is a process-global singleton
//! (rive's `AudioEngine::RuntimeEngine`), created lazily on the first audio event.
//!
//! This module exposes the host **bridge** controls over that singleton: query
//! availability, mute / set the master volume, and release or resume the device.
//! They are free functions (not per-instance) because the engine is global.
//!
//! ```no_run
//! // Half volume for all rive audio, then silence everything:
//! rive_renderer::audio::set_volume(0.5);
//! rive_renderer::audio::stop();
//! ```

use crate::sys;

/// Whether audio was compiled into this build (`--with_rive_audio`). When `false`,
/// every other function here is an inert no-op and a `.riv`'s audio stays silent.
#[must_use]
pub fn is_available() -> bool {
    // SAFETY: takes no arguments and touches no pointers.
    unsafe { sys::rive_audio_is_available() != 0 }
}

/// Opens — or resumes, after [`stop`] — the runtime engine's output device. rive
/// also opens it lazily on the first audio event, so this is optional; use it to
/// pre-warm the device (e.g. to avoid first-sound latency) or to resume playback.
/// Returns `true` if an engine is present (audio available and a device opened).
pub fn start() -> bool {
    // SAFETY: takes no arguments.
    unsafe { sys::rive_audio_start() != 0 }
}

/// Pauses all rive audio and releases the output device. A no-op if no engine has
/// been created yet. Resume with [`start`].
pub fn stop() {
    // SAFETY: takes no arguments.
    unsafe { sys::rive_audio_stop() }
}

/// Sets the master volume for **all** rive audio: `0.0` mutes, `1.0` is unity,
/// values above `1.0` amplify. Creates the engine if needed so the gain sticks for
/// subsequently-played sounds. A no-op when audio is unavailable.
pub fn set_volume(volume: f32) {
    // SAFETY: a plain scalar argument.
    unsafe { sys::rive_audio_set_volume(volume) }
}
