/*
 * rive_shim_audio.cpp — audio engine lifecycle + master volume C ABI.
 *
 * Per-feature shim TU (see rive_shim_internal.hpp + docs/feature-support.md).
 * With `--with_rive_audio=system` (WITH_RIVE_AUDIO), rive owns a miniaudio device
 * that plays audio events / embedded audio assets straight to the OS output during
 * advance — via the lazily-created singleton `AudioEngine::RuntimeEngine()`. So
 * playback is automatic; this TU only exposes the BRIDGE controls a host needs:
 *   - is_available: was audio compiled in?
 *   - start / stop:  open / pause the device (resume / mute-all + release).
 *   - set_volume:    master gain for all rive audio (0 = mute, 1 = unity).
 *
 * Built without WITH_RIVE_AUDIO (audio off), the entry points still exist (stable
 * ABI) but report unavailable / no-op, so the Rust layer links identically and can
 * degrade gracefully.
 */
#include "rive_shim_internal.hpp"

#ifdef WITH_RIVE_AUDIO
#include "rive/audio/audio_engine.hpp" // AudioEngine (+ the global `ma_engine` typedef)

// miniaudio's master-volume setter. Forward-declared (C linkage) rather than
// dragging in the ~76k-line miniaudio.h for one call: `ma_engine` is already a
// global incomplete type from audio_engine.hpp, and ma_result is an int-sized
// enum, so this matches the real `ma_result ma_engine_set_volume(ma_engine*,
// float)` ABI exactly. libminiaudio.a provides the definition.
extern "C" int ma_engine_set_volume(ma_engine* engine, float volume);
#endif

// Whether audio was compiled in (`--with_rive_audio`). 1 = available, 0 = built
// without audio (the other audio entry points are then inert).
extern "C" uint8_t rive_audio_is_available(void)
{
#ifdef WITH_RIVE_AUDIO
    return 1;
#else
    return 0;
#endif
}

// Ensure the runtime audio engine exists and its device is started (opens the OS
// output). rive also creates it lazily on the first audio event; calling this
// pre-warms the device (e.g. to confirm it opens, or to resume after stop()).
// Returns 1 if an engine is present, 0 if audio is unavailable / no device opened.
extern "C" uint8_t rive_audio_start(void)
{
#ifdef WITH_RIVE_AUDIO
    auto engine = rive::AudioEngine::RuntimeEngine(true);
    if (engine == nullptr)
        return 0;
    engine->start();
    return 1;
#else
    return 0;
#endif
}

// Stop the runtime engine's device (silences all rive audio and releases the OS
// output). Does NOT create an engine just to stop it — a no-op if none exists.
// Pair with rive_audio_start to resume.
extern "C" void rive_audio_stop(void)
{
#ifdef WITH_RIVE_AUDIO
    auto engine = rive::AudioEngine::RuntimeEngine(false);
    if (engine != nullptr)
        engine->stop();
#endif
}

// Master volume for ALL rive audio: 0.0 = silent (mute), 1.0 = unity, > 1.0
// amplifies. Applies to the runtime engine's miniaudio engine; creates the engine
// if needed so the setting sticks for subsequently-played sounds. No-op when audio
// is unavailable.
extern "C" void rive_audio_set_volume(float volume)
{
#ifdef WITH_RIVE_AUDIO
    auto engine = rive::AudioEngine::RuntimeEngine(true);
    if (engine != nullptr && engine->engine() != nullptr)
        ma_engine_set_volume(engine->engine(), volume);
#else
    (void)volume;
#endif
}
