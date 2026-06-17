//! Interactive `.riv` viewer whose audio is routed into **Bevy's own audio graph**
//! (the `audio-external` feature). Opens a window, plays the default state machine,
//! and forwards the mouse **cursor + left button** so pointer Listeners fire — drag
//! the wheel to spin it and hear it. Unlike [`interactive`](interactive.rs) (system
//! mode, where rive owns its own OS device), here rive owns **no** device: its mixed
//! PCM is pulled by a [`RiveAudioStream`] `Decodable` source and played through
//! `bevy_audio` (rodio + cpal) like any other Bevy sound — so it mixes under Bevy's
//! `GlobalVolume` alongside the rest of your game's audio.
//!
//! Run (from the workspace root):
//!   cargo run -p bevy-rive --example interactive_external_audio --features floor,audio-external
//!
//! Environment knobs:
//!   RIVE_RIV=9939-18941-big-wheel-demo.riv  which file (default: the big-wheel demo)
//!   RIVE_SIZE=600                           square render resolution (default 600)
//!   RIVE_SPEED=1.0                          state-machine speed (0 = frozen)
//!
//! In the window: **drag** to spin / interact, **M** toggles mute, **↑ / ↓** change
//! the master volume. The HUD logs `rive PCM peak` once a second — it rises above 0
//! when a `.riv`'s audio is flowing through Bevy's graph, confirming the routing.
//!
//! On WSL2, route wgpu onto the real GPU (Dozen) for smooth playback; the window
//! appears on the Windows desktop via WSLg, and Bevy's audio device (cpal → ALSA →
//! WSLg → Windows) plays the sound.

use bevy::core_pipeline::tonemapping::{DebandDither, Tonemapping};
use bevy::prelude::*;
use bevy::winit::WinitSettings;

use bevy_rive::{
    monitor_peak, RiveAnimation, RiveAudio, RiveExternalAudioPlugin, RiveFile, RivePlugin,
    RivePointer, RiveTarget,
};

#[derive(Resource)]
struct Cfg {
    riv: String,
    size: u32,
    speed: f32,
}

/// Marks the entity carrying the `.riv` (the off-screen render source).
#[derive(Component)]
struct RiveEntity;

fn main() {
    let riv = std::env::var("RIVE_RIV")
        .unwrap_or_else(|_| "9939-18941-big-wheel-demo.riv".into())
        .trim_start_matches("assets/")
        .to_string();
    let size = std::env::var("RIVE_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(600);
    let speed = std::env::var("RIVE_SPEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1.0_f32);

    let asset_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets").to_string();
    let title = format!("rive external audio — {riv} (drag · audio via bevy_audio · M mute · ↑↓ volume)");

    App::new()
        .add_plugins(
            DefaultPlugins
                .set(AssetPlugin {
                    file_path: asset_path,
                    ..default()
                })
                .set(WindowPlugin {
                    primary_window: Some(Window {
                        title,
                        ..default()
                    }),
                    ..default()
                }),
        )
        .add_plugins(RivePlugin)
        // Route rive's mixed PCM into Bevy's audio graph. MUST be added after
        // DefaultPlugins (it relies on Bevy's `AudioPlugin`). Spawns the player that
        // streams the `.riv`'s audio through `bevy_audio`.
        .add_plugins(RiveExternalAudioPlugin)
        // WSLg focus is flaky; keep frames flowing whether the window is focused or not.
        .insert_resource(WinitSettings::continuous())
        .insert_resource(ClearColor(Color::srgb(0.10, 0.10, 0.12)))
        // rive's master gain over the mix (composed with Bevy's GlobalVolume); live from frame 1.
        .insert_resource(RiveAudio::default())
        .insert_resource(Cfg { riv, size, speed })
        .add_systems(Startup, setup)
        .add_systems(
            Update,
            (attach_display, feed_pointer, audio_controls, log_peak),
        )
        .run();
}

fn setup(mut commands: Commands, assets: Res<AssetServer>, cfg: Res<Cfg>) {
    commands.spawn((Camera2d, Tonemapping::None, DebandDither::Disabled, Msaa::Off));

    let handle: Handle<RiveFile> = assets.load(cfg.riv.clone());
    let mut anim = RiveAnimation::new(handle);
    anim.speed = cfg.speed;
    commands.spawn((
        anim,
        RiveTarget::new(cfg.size, cfg.size),
        RivePointer::default(),
        RiveEntity,
    ));

    info!(
        "external audio: playing {} @ {}px (speed {}) — drag to interact; audio routes through bevy_audio",
        cfg.riv, cfg.size, cfg.speed
    );
}

/// Spawns the display sprite once the plugin has written the real image handle back
/// into `RiveTarget` (see [`interactive`](interactive.rs) for the full rationale).
fn attach_display(
    mut commands: Commands,
    query: Query<&RiveTarget, With<RiveEntity>>,
    mut done: Local<bool>,
) {
    if *done {
        return;
    }
    let Ok(target) = query.single() else {
        return;
    };
    if target.image == Handle::default() {
        return;
    }
    commands.spawn((Sprite::from_image(target.image.clone()), Transform::IDENTITY));
    *done = true;
    info!("external audio: rendering — drag on it to interact, listen for audio");
}

/// Maps the OS cursor into the face's target-pixel space and writes it to
/// [`RivePointer`] each frame, with the left button as `primary_down` (identical to
/// the [`interactive`](interactive.rs) example — see it for the coordinate math).
fn feed_pointer(
    window: Single<&Window>,
    camera: Single<(&Camera, &GlobalTransform)>,
    mouse: Res<ButtonInput<MouseButton>>,
    mut q: Query<&mut RivePointer, With<RiveEntity>>,
    cfg: Res<Cfg>,
) {
    let Ok(mut ptr) = q.single_mut() else {
        return;
    };
    ptr.primary_down = mouse.pressed(MouseButton::Left);

    let Some(cursor) = window.cursor_position() else {
        ptr.pos = None;
        return;
    };
    let (camera, cam_tf) = *camera;
    let Ok(world) = camera.viewport_to_world_2d(cam_tf, cursor) else {
        ptr.pos = None;
        return;
    };
    let s = cfg.size as f32;
    ptr.pos = Some(Vec2::new(world.x + s / 2.0, s / 2.0 - world.y));
}

/// Keyboard control of the [`RiveAudio`] master knob: **M** toggles mute, **↑ / ↓**
/// step the volume (rive's master gain on the PCM before it enters Bevy's graph).
fn audio_controls(keys: Res<ButtonInput<KeyCode>>, mut audio: ResMut<RiveAudio>) {
    if keys.just_pressed(KeyCode::KeyM) {
        audio.muted = !audio.muted;
        info!("audio: muted = {}", audio.muted);
    }
    let mut vol = audio.master_volume;
    if keys.just_pressed(KeyCode::ArrowUp) {
        vol = (vol + 0.1).min(2.0);
    }
    if keys.just_pressed(KeyCode::ArrowDown) {
        vol = (vol - 0.1).max(0.0);
    }
    if vol != audio.master_volume {
        audio.master_volume = vol;
        info!("audio: master_volume = {vol:.1}");
    }
}

/// Logs the peak of the PCM Bevy last pulled from rive, once a second. `> 0` confirms
/// a `.riv`'s audio is reaching Bevy's audio graph (rises when you spin the wheel).
fn log_peak(time: Res<Time>, mut acc: Local<f32>) {
    *acc += time.delta_secs();
    if *acc >= 1.0 {
        *acc = 0.0;
        info!("rive PCM peak (into bevy_audio): {:.4}", monitor_peak());
    }
}
