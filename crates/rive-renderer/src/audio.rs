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
/// subsequently-played sounds. A no-op when audio is unavailable. Valid in both
/// modes — in external mode it scales the mixed PCM the host pulls.
pub fn set_volume(volume: f32) {
    // SAFETY: a plain scalar argument.
    unsafe { sys::rive_audio_set_volume(volume) }
}

/// Host-mixer (external) pull API — only when built with the `audio-external`
/// feature (`--with_rive_audio=external`).
///
/// In external mode rive owns **no** output device; instead the host pulls the
/// mixed interleaved-f32 PCM with [`read_frames`] (or [`sum_frames`]) and routes it
/// into its own mixer / device. The engine clock advances **only as the host
/// reads**, so the host must pull continuously (typically from its audio callback)
/// for playback to progress. Reading from the host's audio thread is safe
/// concurrently with [`StateMachine::advance`](crate::StateMachine::advance) on the
/// main thread (the same producer/consumer split rive's own device thread uses in
/// system mode).
///
/// These functions are gated behind the feature because pulling makes no sense in
/// `system` mode (rive drives its own device there) — a `system`-mode build has no
/// PCM to hand out and the underlying entry points are inert.
#[cfg(feature = "audio-external")]
pub mod external {
    use crate::sys;

    /// Channels in the pulled PCM (interleaved). rive's runtime engine default is 2.
    /// `0` if no engine could be created.
    #[must_use]
    pub fn channels() -> u32 {
        // SAFETY: takes no arguments.
        unsafe { sys::rive_audio_channels() }
    }

    /// Sample rate (Hz) of the pulled PCM. rive's runtime engine default is 48000.
    /// `0` if no engine could be created.
    #[must_use]
    pub fn sample_rate() -> u32 {
        // SAFETY: takes no arguments.
        unsafe { sys::rive_audio_sample_rate() }
    }

    /// Pulls rive's mixed PCM into `out` (interleaved f32; its length should be a
    /// multiple of [`channels`]). Returns the number of **frames** written — the
    /// number of valid samples is `frames * channels()`. When nothing is playing,
    /// rive fills the buffer with silence and returns the full request, so a steady
    /// pull yields a continuous stream. Returns `0` if audio is unavailable.
    pub fn read_frames(out: &mut [f32]) -> usize {
        let ch = channels().max(1) as usize;
        let num_frames = (out.len() / ch) as u64;
        if num_frames == 0 {
            return 0;
        }
        // SAFETY: `out` is a valid, writable slice of `out.len()` f32; we pass its
        // pointer and a frame count that spans at most `num_frames * ch <= out.len()`
        // samples, so the shim writes within bounds.
        let frames_read = unsafe { sys::rive_audio_read_frames(out.as_mut_ptr(), num_frames) };
        frames_read as usize
    }

    /// Mixes (ADDS) rive's PCM into an existing host buffer `out` (interleaved f32),
    /// for summing rive into a buffer that already holds other audio. `out.len()`
    /// should be a multiple of [`channels`]. Returns `true` on success, `false` if
    /// audio is unavailable.
    pub fn sum_frames(out: &mut [f32]) -> bool {
        let ch = channels().max(1) as usize;
        let num_frames = (out.len() / ch) as u64;
        if num_frames == 0 {
            return false;
        }
        // SAFETY: same bounds reasoning as `read_frames`; the shim reads+adds within
        // `num_frames * ch <= out.len()` samples.
        unsafe { sys::rive_audio_sum_frames(out.as_mut_ptr(), num_frames) != 0 }
    }
}
