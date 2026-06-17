//! Interactive `.riv` viewer with **audio** — opens a window, plays the file's
//! default state machine, and forwards the mouse **cursor + left button** into the
//! state machine so its pointer Listeners fire. Drag on the wheel to spin it; the
//! file's audio events play to your speakers as it spins / lands. Built on the
//! `bevy-rive` **floor** (CPU-copy) tier.
//!
//! Run (from the workspace root):
//!   cargo run -p bevy-rive --example interactive --features floor
//!
//! Environment knobs:
//!   RIVE_RIV=9939-18941-big-wheel-demo.riv  which file (default: the big-wheel demo)
//!   RIVE_SIZE=600                           square render resolution (default 600)
//!   RIVE_SPEED=1.0                          state-machine speed (0 = frozen)
//!
//! In the window: **drag** to spin / interact, **M** toggles mute, **↑ / ↓** change
//! the master volume. Audio plays automatically during advance — `--with_rive_audio
//! =system` routes a `.riv`'s audio events straight to the OS output (on WSL2 via
//! WSLg → Windows). The [`RiveAudio`] resource here is the master volume / mute knob.
//!
//! On WSL2, route wgpu onto the real GPU (Dozen) for smooth playback; otherwise it
//! still runs on the software rasterizer, just slowly. The window appears on the
//! Windows desktop via WSLg, and audio comes out of the Windows default device.

use bevy::core_pipeline::tonemapping::{DebandDither, Tonemapping};
use bevy::prelude::*;
use bevy::winit::WinitSettings;

use bevy_rive::{RiveAnimation, RiveAudio, RiveFile, RivePlugin, RivePointer, RiveTarget};

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
    // Bevy's asset root is the `assets/` folder; strip a leading `assets/` the
    // caller may have included in RIVE_RIV.
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

    // This example is a target of `crates/bevy-rive`, so Bevy's default asset root
    // (CARGO_MANIFEST_DIR) points at the crate; anchor it at the workspace `assets/`.
    let asset_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets").to_string();
    let title = format!("rive interactive — {riv} (drag to interact · M mute · ↑↓ volume)");

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
        // WSLg focus is flaky; keep frames flowing whether the window is focused or not.
        .insert_resource(WinitSettings::continuous())
        .insert_resource(ClearColor(Color::srgb(0.10, 0.10, 0.12)))
        // Insert the audio control so its volume / mute is live from the first frame.
        // (Absent, audio still plays at unity; this is the master knob.)
        .insert_resource(RiveAudio::default())
        .insert_resource(Cfg { riv, size, speed })
        .add_systems(Startup, setup)
        .add_systems(Update, (attach_display, feed_pointer, audio_controls))
        .run();
}

fn setup(mut commands: Commands, assets: Res<AssetServer>, cfg: Res<Cfg>) {
    // Color/orientation contract: Tonemapping::None + no Hdr (identity sRGB
    // round-trip), Msaa::Off — so the sprite shows rive's already-final pixels.
    commands.spawn((Camera2d, Tonemapping::None, DebandDither::Disabled, Msaa::Off));

    let handle: Handle<RiveFile> = assets.load(cfg.riv.clone());
    let mut anim = RiveAnimation::new(handle);
    anim.speed = cfg.speed;
    // `RivePointer` starts empty; `feed_pointer` fills it from the cursor.
    commands.spawn((
        anim,
        RiveTarget::new(cfg.size, cfg.size),
        RivePointer::default(),
        RiveEntity,
    ));

    info!(
        "interactive: playing {} @ {}px (speed {}) — drag to interact; audio plays on advance",
        cfg.riv, cfg.size, cfg.speed
    );
}

/// Spawns the display sprite once the plugin has written the real image handle
/// back into `RiveTarget`. Polls each frame (no `Changed` filter) so it is robust
/// to system ordering. `Sprite` re-binds on `AssetEvent::Modified`, so it tracks
/// the floor tier's per-frame re-uploaded image (a cached material would freeze).
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
    info!("interactive: rendering — drag on it to interact, listen for audio");
}

/// Maps the OS cursor into the face's target-pixel space and writes it to
/// [`RivePointer`] each frame, with the left button as `primary_down`. The display
/// sprite is a centered, identity-scaled `Sprite` of size `size`×`size` (see
/// `attach_display`), so: cursor → world (via the camera) → sprite-local pixels.
/// World space is +y up with the origin at the sprite center; rive target-pixel
/// space is top-left origin, +y down — hence the Y flip. `None` when the cursor is
/// outside the window (→ `pointerExit`).
fn feed_pointer(
    window: Single<&Window>,
    camera: Single<(&Camera, &GlobalTransform)>,
    mouse: Res<ButtonInput<MouseButton>>,
    mut q: Query<&mut RivePointer, With<RiveEntity>>,
    cfg: Res<Cfg>,
) {
    let Ok(mut ptr) = q.single_mut() else {
        return; // entity not spawned yet (or >1 — shouldn't happen)
    };
    ptr.primary_down = mouse.pressed(MouseButton::Left);

    let Some(cursor) = window.cursor_position() else {
        ptr.pos = None; // cursor left the window
        return;
    };
    let (camera, cam_tf) = *camera;
    let Ok(world) = camera.viewport_to_world_2d(cam_tf, cursor) else {
        ptr.pos = None;
        return;
    };
    // Centered identity sprite of size `s`×`s`: world origin = sprite center.
    // Target-pixel: x = world.x + s/2 ; y = s/2 - world.y (flip world +y-up).
    let s = cfg.size as f32;
    ptr.pos = Some(Vec2::new(world.x + s / 2.0, s / 2.0 - world.y));
}

/// Keyboard control of the [`RiveAudio`] master knob: **M** toggles mute, **↑ / ↓**
/// step the volume. Mutating the resource re-applies it to the engine (the plugin's
/// `apply_rive_audio` runs on change).
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
