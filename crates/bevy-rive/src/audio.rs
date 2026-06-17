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

// =============================================================================
// External (host-mixer) routing — the `audio-external` feature.
//
// In external mode rive owns no device; this routes its mixed PCM into Bevy's OWN
// audio graph via a `Decodable` source, so a `.riv`'s audio plays through `bevy_audio`
// (rodio + cpal) like any other sound — unified mixing under Bevy's `GlobalVolume`,
// rather than rive opening a separate OS device (the default `system` mode).
// =============================================================================
#[cfg(feature = "audio-external")]
mod external {
    // `AudioPlayer`, `Decodable`, `PlaybackSettings` come via `bevy::prelude`; `Source`
    // and `AddAudioSource` are not in the audio prelude, so name them explicitly.
    use bevy::audio::{AddAudioSource, Source};
    use bevy::prelude::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    // Process-global running peak (abs sample) of the last non-empty PCM block the
    // audio thread pulled — the rive engine is itself a process-global singleton, so a
    // single global mirrors it. Lets an app confirm rive audio is actually flowing
    // through Bevy's graph (peak > 0). Stored as the bits of an f32.
    static LAST_PEAK: AtomicU32 = AtomicU32::new(0);

    /// The most recent absolute peak of host-pulled rive PCM (`0.0` = silence / nothing
    /// playing). A lightweight monitor for confirming a `.riv`'s audio is reaching
    /// Bevy's audio graph in external mode — e.g. log it to see it rise when an
    /// interaction fires a sound.
    #[must_use]
    pub fn monitor_peak() -> f32 {
        f32::from_bits(LAST_PEAK.load(Ordering::Relaxed))
    }

    /// A Bevy [`Asset`] whose audio is rive's whole mixed output (external mode). It is
    /// a single, process-wide stream (the rive engine is global), so one
    /// [`AudioPlayer<RiveAudioStream>`] entity plays everything a `.riv` emits.
    /// [`RiveExternalAudioPlugin`] registers it and spawns that entity for you.
    #[derive(Asset, TypePath, Debug, Clone, Default)]
    pub struct RiveAudioStream;

    /// The rodio source that pulls rive's mixed PCM on the audio thread. Buffers one
    /// ~video-frame block per refill and drains it sample-by-sample.
    #[derive(Debug)]
    pub struct RiveAudioDecoder {
        channels: u16,
        sample_rate: u32,
        buf: Vec<f32>,
        pos: usize,
    }

    impl RiveAudioDecoder {
        fn refill(&mut self) {
            let frames = (self.sample_rate as usize / 60).max(1);
            self.buf.resize(frames * self.channels as usize, 0.0);
            let n = rive_renderer::audio::external::read_frames(&mut self.buf);
            if n > 0 {
                self.buf.truncate(n * self.channels as usize);
                let peak = self.buf.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
                LAST_PEAK.store(peak.to_bits(), Ordering::Relaxed);
            }
            // n == 0 (no engine / nothing playing): keep the zeroed block as silence so
            // the stream never ends and we don't hammer the FFI per sample.
            self.pos = 0;
        }
    }

    impl Iterator for RiveAudioDecoder {
        type Item = f32;
        fn next(&mut self) -> Option<f32> {
            if self.pos >= self.buf.len() {
                self.refill();
            }
            let s = self.buf.get(self.pos).copied().unwrap_or(0.0);
            self.pos += 1;
            // Endless stream: always `Some`, so the sink plays rive's mix continuously
            // (silence when nothing is playing) and never gets cleaned up.
            Some(s)
        }
    }

    impl Source for RiveAudioDecoder {
        fn current_frame_len(&self) -> Option<usize> {
            None // unknown / endless stream
        }
        fn channels(&self) -> u16 {
            self.channels
        }
        fn sample_rate(&self) -> u32 {
            self.sample_rate
        }
        fn total_duration(&self) -> Option<Duration> {
            None // endless
        }
    }

    impl Decodable for RiveAudioStream {
        type DecoderItem = f32;
        type Decoder = RiveAudioDecoder;

        fn decoder(&self) -> RiveAudioDecoder {
            // Bevy calls this on the MAIN thread (play_queued_audio_system) when the
            // player starts. Querying the engine here creates the global runtime engine
            // on the main thread BEFORE the audio thread first pulls — so creation never
            // races advance's `play()` calls. rodio resamples/remixes to the device, so
            // we just report rive's native format.
            let channels = rive_renderer::audio::external::channels().clamp(1, 8) as u16;
            let sample_rate = rive_renderer::audio::external::sample_rate().max(8_000);
            RiveAudioDecoder {
                channels,
                sample_rate,
                buf: Vec::new(),
                pos: 0,
            }
        }
    }

    /// Routes rive's audio into Bevy's audio graph (external mode). Registers
    /// [`RiveAudioStream`] as an audio source and spawns one player that streams rive's
    /// mixed PCM through `bevy_audio`. **Add it AFTER `DefaultPlugins`** (it needs
    /// Bevy's `AudioPlugin`). The [`RiveAudio`](super::RiveAudio) resource still applies
    /// (as rive's master gain on the mix, composed with Bevy's `GlobalVolume`).
    #[derive(Default, Debug)]
    pub struct RiveExternalAudioPlugin;

    impl Plugin for RiveExternalAudioPlugin {
        fn build(&self, app: &mut App) {
            app.add_audio_source::<RiveAudioStream>();
            app.add_systems(Startup, spawn_rive_audio_stream);
        }
    }

    fn spawn_rive_audio_stream(
        mut commands: Commands,
        mut assets: ResMut<Assets<RiveAudioStream>>,
    ) {
        let handle = assets.add(RiveAudioStream);
        // ONCE (not LOOP): the source is already endless, so it plays forever; the sink
        // is never finished, so `cleanup_finished_audio` leaves the entity in place.
        commands.spawn((AudioPlayer(handle), PlaybackSettings::ONCE));
    }
}

#[cfg(feature = "audio-external")]
pub use external::{monitor_peak, RiveAudioDecoder, RiveAudioStream, RiveExternalAudioPlugin};
