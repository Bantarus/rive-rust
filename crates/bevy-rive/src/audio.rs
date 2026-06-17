//! Audio control — master volume / mute over rive's global audio engine.
//!
//! rive plays a `.riv`'s audio events / embedded audio to the OS output
//! **automatically** during advance, in BOTH tiers (the crate is built with
//! `--with_rive_audio=system`). There is no per-sound API; this module adds the
//! optional [`RiveAudio`] resource to control the master volume / mute that
//! playback. The engine is process-global (one miniaudio device), so this is a
//! single resource, not a per-entity component.
//!
//! **Absent by default.** A consumer that never inserts [`RiveAudio`] gets audio at
//! unity volume, and the audio device is opened lazily on the first sound — so a
//! silent app opens no device. Insert [`RiveAudio`] only to take control; doing so
//! opens the device eagerly (you asked to drive it).

use bevy::prelude::*;

/// Master audio control for ALL rive audio (process-global). Insert it to set the
/// volume or mute every `.riv`'s audio; mutate it to change them at runtime. The
/// plugin applies changes through [`rive_renderer::audio`]. Not inserted by
/// default (see the [module docs](self)).
///
/// ```no_run
/// # use bevy::prelude::*;
/// # use bevy_rive::prelude::*;
/// # fn s(mut commands: Commands) {
/// commands.insert_resource(RiveAudio { master_volume: 0.5, muted: false });
/// # }
/// ```
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct RiveAudio {
    /// Master volume: `0.0` = silent, `1.0` = unity, `> 1.0` amplifies.
    pub master_volume: f32,
    /// Mute toggle — silences all rive audio without disturbing `master_volume`.
    pub muted: bool,
}

impl Default for RiveAudio {
    fn default() -> Self {
        Self {
            master_volume: 1.0,
            muted: false,
        }
    }
}

/// Applies [`RiveAudio`] to the global engine whenever it changes. Takes the
/// resource as `Option`, so when a consumer never inserts it this is a cheap no-op
/// and NO audio device is opened until a sound actually plays. Registered in both
/// tiers' plugin `Update` schedule.
pub(crate) fn apply_rive_audio(audio: Option<Res<RiveAudio>>) {
    let Some(audio) = audio else { return };
    // `is_changed()` is true the frame after insertion and on every mutation, so the
    // engine tracks the resource without a per-frame FFI call when nothing changed.
    if audio.is_changed() {
        let volume = if audio.muted { 0.0 } else { audio.master_volume.max(0.0) };
        rive_renderer::audio::set_volume(volume);
    }
}
