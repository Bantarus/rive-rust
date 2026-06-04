//! Live viewer for the **Nimai** face `.riv` — opens a window, plays the file's
//! default state machine each frame, and **forwards the mouse cursor** into the
//! state machine so its pointer **Listeners** fire: the eyes and head joystick
//! follow your cursor. Built on the `bevy-rive` **floor** (CPU-copy) tier.
//!
//! Run (from the workspace root):
//!   cargo run -p bevy-rive --example nimai_face --features floor
//!
//! Environment knobs:
//!   RIVE_RIV=nimai_published.riv   which file to play (default). Any `.riv` with
//!                                  pointer Listeners / a pointer-driven joystick
//!                                  will track the cursor.
//!   RIVE_SIZE=512                  square render resolution (default 512)
//!   RIVE_SPEED=1.0                 state-machine speed (0 = frozen, but pointer
//!                                  input still updates listeners on advance)
//!
//! How the cursor reaches the face: this app owns the display (a centered
//! `Sprite` of the rive image), so it maps the OS cursor into the face's
//! **target-pixel** space and writes it to the [`RivePointer`] component each
//! frame. `bevy-rive` forwards that to the native state machine (inverting the
//! same fit/alignment it draws with), so hit-testing lines up with the pixels.
//!
//! On WSL2, route wgpu onto the real GPU (Dozen) rather than llvmpipe for smooth
//! playback; otherwise it still runs, just slowly, on the software rasterizer.

use bevy::core_pipeline::tonemapping::{DebandDither, Tonemapping};
use bevy::prelude::*;
use bevy::winit::WinitSettings;

use bevy_rive::{RiveAnimation, RiveFile, RivePlugin, RivePointer, RiveTarget};

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
        .unwrap_or_else(|_| "nimai_published.riv".into())
        .trim_start_matches("assets/")
        .to_string();
    let size = std::env::var("RIVE_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(512);
    let speed = std::env::var("RIVE_SPEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1.0_f32);

    // This example is a target of `crates/bevy-rive`, so Bevy's default asset root
    // (CARGO_MANIFEST_DIR) points at the crate; anchor it at the workspace `assets/`.
    let asset_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets").to_string();
    let title = format!("nimai face — {riv} (move the cursor)");

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
        .insert_resource(Cfg { riv, size, speed })
        .add_systems(Startup, setup)
        .add_systems(Update, (attach_display, feed_pointer))
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
        "nimai_face: playing {} @ {}px (speed {}) — move the cursor to drive the face",
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
    info!("nimai_face: face is rendering");
}

/// Maps the OS cursor into the face's target-pixel space and writes it to
/// [`RivePointer`] each frame. The display sprite is a centered, identity-scaled
/// `Sprite` of size `size`×`size` (see `attach_display`), so: cursor → world (via
/// the camera) → sprite-local pixels. World space is +y up with the origin at the
/// sprite's center; rive target-pixel space is top-left origin, +y down — hence
/// the Y flip. `None` when the cursor is outside the window (→ `pointerExit`).
fn feed_pointer(
    window: Single<&Window>,
    camera: Single<(&Camera, &GlobalTransform)>,
    mouse: Res<ButtonInput<MouseButton>>,
    mut q: Query<&mut RivePointer, With<RiveEntity>>,
    cfg: Res<Cfg>,
) {
    let Ok(mut ptr) = q.single_mut() else {
        return; // face not spawned yet (or >1 — shouldn't happen)
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
    // A square target means W == H == size. Off-sprite cursors map outside
    // [0, s] and simply miss every listener (a harmless `pointerMove`).
    let s = cfg.size as f32;
    ptr.pos = Some(Vec2::new(world.x + s / 2.0, s / 2.0 - world.y));
}
