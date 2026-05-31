//! Milestone 1a deliverable: a `.riv` animates on a sprite, driven each frame by
//! the native Rive Renderer via the `bevy-rive` CPU-copy bridge.
//!
//! Usage (interactive — opens a window showing the animation):
//!   cargo run --example sprite_riv
//!
//! Environment:
//!   RIVE_RIV=assets/octopus_loop.riv   which file to play (default)
//!   RIVE_CAPTURE=cap.png               headless-ish verify: after a few frames,
//!                                      write `cap.png` (composited window) and
//!                                      `cap.png.offscreen.png` (the raw Image,
//!                                      straight RGBA), then exit
//!   RIVE_CAPTURE_FRAMES=6              warm-up frames before capture (default 6)
//!
//! The frozen color/orientation contract requires `Tonemapping::None` + no `Hdr`
//! on the camera (so the sRGB sample->output round-trip is an identity) and
//! `Msaa::Off` for a clean pixel comparison against the M0/M1.0 PNG references.

use bevy::core_pipeline::tonemapping::{DebandDither, Tonemapping};
use bevy::prelude::*;
use bevy::render::view::screenshot::{save_to_disk, Screenshot};
use bevy::winit::WinitSettings;

use bevy_rive::{RiveAnimation, RiveFile, RivePlugin, RiveTarget};

#[derive(Resource)]
struct Cfg {
    riv: String,
    size: u32,
    capture: Option<String>,
    warmup: u32,
    speed: f32,
}

#[derive(Resource, Default)]
struct CaptureState {
    /// Frames counted since the display quad started rendering.
    frames: u32,
    /// Whether the screenshots have been requested.
    requested: bool,
}

/// Marks the entity carrying the `.riv`.
#[derive(Component)]
struct RiveEntity;

/// Marks the spawned display quad, so capture waits until it is actually
/// rendering before screenshotting the window (the offscreen Image fills a few
/// frames before the quad composites; on fast GPUs a naive frame count races it).
#[derive(Component)]
struct DisplayQuad;

fn main() {
    // Asset paths are relative to Bevy's `assets/` root, so strip a leading
    // `assets/` if the caller included it.
    let riv = std::env::var("RIVE_RIV")
        .unwrap_or_else(|_| "octopus_loop.riv".into())
        .trim_start_matches("assets/")
        .to_string();
    let capture = std::env::var("RIVE_CAPTURE").ok();
    let warmup = std::env::var("RIVE_CAPTURE_FRAMES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6);
    // Test knob (default 1.0 = realtime). RIVE_SPEED=0 freezes the state machine at
    // its initial pose, giving a deterministic, pose-matched frame for the
    // M1a-vs-M1b transparent-content diff.
    let speed = std::env::var("RIVE_SPEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1.0_f32);

    // The example lives in the workspace-root `examples/` dir but is compiled as a
    // target of `crates/bevy-rive`, so Bevy's default asset root (CARGO_MANIFEST_DIR)
    // points at the crate. Anchor it at the workspace `assets/` folder instead.
    let asset_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets").to_string();

    App::new()
        .add_plugins(DefaultPlugins.set(AssetPlugin {
            file_path: asset_path,
            ..default()
        }))
        .add_plugins(RivePlugin)
        // WSLg focus is flaky; keep frames flowing whether focused or not.
        .insert_resource(WinitSettings::continuous())
        .insert_resource(Cfg {
            riv,
            size: 512,
            capture,
            warmup,
            speed,
        })
        .init_resource::<CaptureState>()
        .add_systems(Startup, setup)
        .add_systems(Update, (attach_display, drive_capture).chain())
        .run();
}

fn setup(mut commands: Commands, assets: Res<AssetServer>, cfg: Res<Cfg>) {
    // Camera pins for a reference-faithful composite: Tonemapping::None (else the
    // camera's tonemap LUT alters rive's already-final color), no Hdr (keeps the
    // sRGB round-trip), Msaa::Off (clean pixel diff). Camera2d already defaults
    // Tonemapping::None + DebandDither::Disabled; set them explicitly to make the
    // invariant local.
    commands.spawn((
        Camera2d,
        Tonemapping::None,
        DebandDither::Disabled,
        Msaa::Off,
    ));

    let handle: Handle<RiveFile> = assets.load(cfg.riv.clone());
    let mut anim = RiveAnimation::new(handle);
    anim.speed = cfg.speed;
    commands.spawn((anim, RiveTarget::new(cfg.size, cfg.size), RiveEntity));
    // `attach_display` spawns the textured quad once the plugin writes the real
    // image handle back into RiveTarget.
}

/// Spawns the display quad once the plugin has allocated the target image.
/// Polls each frame (no `Changed` filter) so it is robust to system ordering.
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

    // Bevy's built-in `Sprite` re-binds its texture on `AssetEvent::Modified`, so
    // it tracks the per-frame re-uploaded Image. (A custom `Material2d` / 3D
    // `StandardMaterial` caches its bind group and would freeze on frame 1 under
    // M1a's per-frame GpuImage recreation — those need M1b's stable shared
    // texture. So the M1a display is `Sprite`.)
    commands.spawn((
        Sprite::from_image(target.image.clone()),
        Transform::IDENTITY,
        DisplayQuad,
    ));
    *done = true;
}

/// In capture mode: after `warmup` frames, dump the offscreen Rive texture (RGBA,
/// deterministic) and request a composited-window screenshot, then exit.
fn drive_capture(
    mut commands: Commands,
    mut state: ResMut<CaptureState>,
    cfg: Res<Cfg>,
    images: Res<Assets<Image>>,
    query: Query<&RiveTarget, With<RiveEntity>>,
    quad: Query<(), With<DisplayQuad>>,
    mut exit: MessageWriter<AppExit>,
) {
    let Some(path) = cfg.capture.as_ref() else {
        return;
    };
    let Ok(target) = query.single() else {
        return;
    };
    // Only start the clock once the display quad exists, so the window screenshot
    // is taken well after the sprite is actually compositing (GPU-speed-agnostic).
    if target.image == Handle::default() || quad.is_empty() {
        return;
    }
    state.frames += 1;

    if state.frames >= cfg.warmup && !state.requested {
        // (a) The raw Image the plugin filled — straight RGBA, contract-faithful.
        if let Some(image) = images.get(&target.image) {
            if let Some(data) = image.data.as_ref() {
                let out = format!("{path}.offscreen.png");
                match image::save_buffer(
                    &out,
                    data,
                    image.width(),
                    image.height(),
                    image::ExtendedColorType::Rgba8,
                ) {
                    Ok(()) => info!("rive: wrote {out}"),
                    Err(e) => warn!("rive: offscreen save failed: {e}"),
                }
            }
        }
        // (b) Composited window — the sprite as actually displayed.
        commands
            .spawn(Screenshot::primary_window())
            .observe(save_to_disk(path.clone()));
        state.requested = true;
    }

    // Give the async screenshot readback + file write a generous margin to flush.
    if state.requested && state.frames >= cfg.warmup + 30 {
        info!("rive: capture complete, exiting");
        exit.write(AppExit::Success);
    }
}
