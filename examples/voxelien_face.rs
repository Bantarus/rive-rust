//! Live viewer for the Voxelien face `.riv` — opens a window and plays the file's
//! default state machine each frame (including scripted nodes like `BallBreath`)
//! via the `bevy-rive` **floor** (CPU-copy) tier. This is the simplest possible
//! "see it render" app: native Rive Renderer -> a Bevy `Image` -> a `Sprite`.
//!
//! Run (from the workspace root):
//!   cargo run -p bevy-rive --example voxelien_face --features floor
//!
//! Environment knobs:
//!   RIVE_RIV=voxelien_face_published.riv   which file to play. Default is the
//!                                          PUBLISHED (signed) face — its scripts
//!                                          pass rive's signature gate. Point this
//!                                          at `voxelien_face.riv` (the backup
//!                                          export) to see the unsigned case.
//!   RIVE_SIZE=512                          square render resolution (default 512)
//!   RIVE_SPEED=1.0                         state-machine speed (0 = frozen pose)
//!
//! Scripted motion (BallBreath) only ticks when `rive-renderer-sys` is built with
//! `--with_rive_scripting` (set in its `build.rs`) AND the `.riv` is signed
//! (publish-export, not backup-export). Rendering of the face itself does not need
//! either — so a blank-but-present face here means the script gate, not the render.
//!
//! On WSL2, route wgpu onto the real GPU (Dozen) rather than llvmpipe for smooth
//! playback; otherwise it still runs, just slowly, on the software rasterizer.

use bevy::core_pipeline::tonemapping::{DebandDither, Tonemapping};
use bevy::prelude::*;
use bevy::winit::WinitSettings;

use bevy_rive::{RiveAnimation, RiveFile, RivePlugin, RiveTarget};

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
        .unwrap_or_else(|_| "voxelien_face_published.riv".into())
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
    let title = format!("voxelien face — {riv}");

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
        .add_systems(Update, attach_display)
        .run();
}

fn setup(mut commands: Commands, assets: Res<AssetServer>, cfg: Res<Cfg>) {
    // Color/orientation contract: Tonemapping::None + no Hdr (identity sRGB
    // round-trip), Msaa::Off — so the sprite shows rive's already-final pixels.
    commands.spawn((Camera2d, Tonemapping::None, DebandDither::Disabled, Msaa::Off));

    let handle: Handle<RiveFile> = assets.load(cfg.riv.clone());
    let mut anim = RiveAnimation::new(handle);
    anim.speed = cfg.speed;
    commands.spawn((anim, RiveTarget::new(cfg.size, cfg.size), RiveEntity));

    info!(
        "voxelien_face: playing {} @ {}px (speed {})",
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
    info!("voxelien_face: face is rendering");
}
