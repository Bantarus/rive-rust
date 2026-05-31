//! Milestone 1b deliverable: a `.riv` animates on a sprite, driven each frame by
//! the native Rive Renderer rendering **directly into a wgpu-allocated `VkImage`**
//! (zero copy — no per-frame CPU readback), via the `bevy-rive` `zero_copy` tier.
//!
//! Build/run requires the `zero_copy` feature **and native Vulkan** (set
//! `WGPU_BACKEND=vulkan`). On a GPU with `VK_EXT_fragment_shader_interlock`
//! (e.g. NVIDIA) rive runs its clean raster-order PLS path; elsewhere it falls
//! back to the atomic path (still correct).
//!
//!   WGPU_BACKEND=vulkan cargo run -p bevy-rive --features zero_copy \
//!       --example sprite_riv_zerocopy
//!
//! Environment (mirrors `sprite_riv`):
//!   RIVE_RIV=octopus_loop.riv     which file to play (default)
//!   RIVE_CAPTURE=cap.png          after a few frames, screenshot the window then exit
//!   RIVE_CAPTURE_FRAMES=6         warm-up frames before capture (default 6)
//!
//! The **display path is identical to M1a**: a Bevy `Sprite` on
//! `RiveTarget.image`. That is the whole point of the uniform seam — only the
//! fill mechanism (render-graph node + shared `VkImage`) differs from M1a.
//!
//! NOTE the offscreen-dump from `sprite_riv` is omitted here: M1b's display image
//! is GPU-only (`data: None`), so there are no CPU bytes to dump. Verification is
//! the composited-window screenshot (and, on native HW, comparing it to M1a).

use bevy::core_pipeline::tonemapping::{DebandDither, Tonemapping};
use bevy::prelude::*;
use bevy::render::view::screenshot::{save_to_disk, Screenshot};
use bevy::winit::WinitSettings;

use bevy_rive::{
    install_interlock_device_callback, RiveAnimation, RiveFile, RiveTarget, RiveZeroCopyPlugin,
};

#[derive(Resource)]
struct Cfg {
    riv: String,
    size: u32,
    capture: Option<String>,
    warmup: u32,
}

#[derive(Resource, Default)]
struct CaptureState {
    frames: u32,
    requested: bool,
}

#[derive(Component)]
struct RiveEntity;

#[derive(Component)]
struct DisplayQuad;

fn main() {
    let riv = std::env::var("RIVE_RIV")
        .unwrap_or_else(|_| "octopus_loop.riv".into())
        .trim_start_matches("assets/")
        .to_string();
    let capture = std::env::var("RIVE_CAPTURE").ok();
    let warmup = std::env::var("RIVE_CAPTURE_FRAMES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6);

    let asset_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets").to_string();

    let mut app = App::new();

    // M1b device sharing: register the Vulkan device-creation callback that adds
    // rive's interlock extension to the device Bevy builds. MUST run before
    // DefaultPlugins (RenderPlugin reads the settings resource during build).
    install_interlock_device_callback(&mut app);

    app.add_plugins(DefaultPlugins.set(AssetPlugin {
        file_path: asset_path,
        ..default()
    }))
    // The zero-copy plugin registers the `.riv` asset + loader itself, plus the
    // render-world bridge. Use it INSTEAD of RivePlugin (no CPU-copy systems).
    .add_plugins(RiveZeroCopyPlugin)
    .insert_resource(WinitSettings::continuous())
    .insert_resource(Cfg {
        riv,
        size: 512,
        capture,
        warmup,
    })
    .init_resource::<CaptureState>()
    .add_systems(Startup, setup)
    .add_systems(Update, (attach_display, drive_capture).chain())
    .run();
}

fn setup(mut commands: Commands, assets: Res<AssetServer>, cfg: Res<Cfg>) {
    // Same camera pins as M1a: Tonemapping::None + no Hdr (sRGB round-trip is an
    // identity) + Msaa::Off (clean pixel diff) — the display contract is shared.
    commands.spawn((
        Camera2d,
        Tonemapping::None,
        DebandDither::Disabled,
        Msaa::Off,
    ));

    let handle: Handle<RiveFile> = assets.load(cfg.riv.clone());
    commands.spawn((
        RiveAnimation::new(handle),
        RiveTarget::new(cfg.size, cfg.size),
        RiveEntity,
    ));
}

/// Spawns the display sprite once the plugin has allocated the target image —
/// identical to M1a. The image is filled by the render-graph node each frame
/// (in place, a stable texture), so a `Sprite` displays the live animation.
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
    commands.spawn((
        Sprite::from_image(target.image.clone()),
        Transform::IDENTITY,
        DisplayQuad,
    ));
    *done = true;
}

/// In capture mode: after `warmup` frames (gated on the display sprite existing),
/// screenshot the composited window, then exit. No offscreen dump — M1b's image
/// is GPU-only.
fn drive_capture(
    mut commands: Commands,
    mut state: ResMut<CaptureState>,
    cfg: Res<Cfg>,
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
    if target.image == Handle::default() || quad.is_empty() {
        return;
    }
    state.frames += 1;

    if state.frames >= cfg.warmup && !state.requested {
        commands
            .spawn(Screenshot::primary_window())
            .observe(save_to_disk(path.clone()));
        state.requested = true;
    }

    if state.requested && state.frames >= cfg.warmup + 30 {
        info!("rive: capture complete, exiting");
        exit.write(AppExit::Success);
    }
}
