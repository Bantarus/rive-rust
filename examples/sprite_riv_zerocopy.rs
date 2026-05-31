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
//!   RIVE_INSTANCES=N              spawn N independent rive instances in a grid
//!                                 (default 1) — the M2a multi-instance perf regime;
//!                                 each renders into its own shared VkImage + sprite.
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
    speed: f32,
    /// M2a: number of independent rive instances to spawn (grid-laid). 1 by
    /// default; the multi-instance perf regime uses 8 / 32 / 128.
    instances: u32,
    /// M2.0: when `RIVE_PERF` is set, auto-exit after this many frames so a perf
    /// run terminates on its own once the render-world collector has logged its
    /// summary (it summarizes after ~warmup + `RIVE_PERF_FRAMES` rendered frames).
    /// `None` outside perf mode (the app runs until closed / capture exit).
    perf_exit_frames: Option<u32>,
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

/// Grid slot index of a rive instance (M2a multi-instance), used to lay its
/// display sprite out in a grid.
#[derive(Component)]
struct RiveSlot(u32);

/// Marks a rive entity whose display sprite has been spawned, so `attach_display`
/// attaches each instance exactly once.
#[derive(Component)]
struct DisplayAttached;

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
    // Test knob (default 1.0 = realtime). RIVE_SPEED=0 freezes the state machine at
    // its initial pose, giving a deterministic, pose-matched frame for the
    // M1a-vs-M1b transparent-content diff (and a fixed per-frame cost for perf).
    let speed = std::env::var("RIVE_SPEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1.0_f32);
    // M2a multi-instance perf regime: spawn N independent rive instances. Clamped
    // to a sane ceiling so a typo can't try to allocate thousands of 512² targets.
    let instances = std::env::var("RIVE_INSTANCES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1u32)
        .clamp(1, 1024);
    // M2.0 perf mode (RIVE_PERF): the render-world collector logs a summary after
    // ~30 warm-up + RIVE_PERF_FRAMES (default 300) rendered frames; give the app a
    // frame budget past that so the run self-terminates with the summary printed.
    let perf_exit_frames = std::env::var_os("RIVE_PERF").map(|_| {
        let target: u32 = std::env::var("RIVE_PERF_FRAMES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(300);
        // 30 (collector warm-up) + target + 120 margin for main/render frame skew.
        target.saturating_add(150)
    });

    let asset_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets").to_string();

    let mut app = App::new();

    // M1b device sharing: register the Vulkan device-creation callback that adds
    // rive's interlock extension to the device Bevy builds. MUST run before
    // DefaultPlugins (RenderPlugin reads the settings resource during build).
    install_interlock_device_callback(&mut app);

    app.add_plugins(
        DefaultPlugins
            .set(AssetPlugin {
                file_path: asset_path,
                ..default()
            })
            // CORRECTNESS-TIER CHOICE (deliberate, reversible): disable pipelined
            // rendering so the render world — which owns rive's `!Send` handles as a
            // NonSend resource — runs on the main thread. This drops main/render
            // overlap but eliminates every cross-thread hazard (no `unsafe Send`,
            // plain `Rc` refcount). M2 may restore pipelining with a validated
            // cross-thread strategy that makes the resource *drop* sound, not just
            // the move (see the rive-renderer threading note).
            .disable::<bevy::render::pipelined_rendering::PipelinedRenderingPlugin>(),
    )
    // The zero-copy plugin registers the `.riv` asset + loader itself, plus the
    // render-world bridge. Use it INSTEAD of RivePlugin (no CPU-copy systems).
    .add_plugins(RiveZeroCopyPlugin)
    .insert_resource(WinitSettings::continuous())
    .insert_resource(Cfg {
        riv,
        size: 512,
        capture,
        warmup,
        speed,
        instances,
        perf_exit_frames,
    })
    .init_resource::<CaptureState>()
    .add_systems(Startup, setup)
    .add_systems(
        Update,
        (attach_display, drive_capture, drive_perf_exit).chain(),
    )
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
    // M2a: spawn N independent rive instances (each its own artboard, state
    // machine, shared VkImage, and display image). N=1 is the M1b/M2.0 baseline;
    // 8/32/128 exercise the multi-instance perf regime. All share the single loaded
    // RiveFile asset (a cheap handle clone); the render node builds per-entity
    // native objects on first sight.
    info!("rive zero-copy: spawning {} instance(s)", cfg.instances);
    for i in 0..cfg.instances {
        let mut anim = RiveAnimation::new(handle.clone());
        anim.speed = cfg.speed;
        commands.spawn((
            anim,
            RiveTarget::new(cfg.size, cfg.size),
            RiveEntity,
            RiveSlot(i),
        ));
    }
}

/// Spawns a display sprite for each rive instance once the plugin has allocated
/// its target image — laid out in a grid (M2a multi-instance). Each instance is
/// attached exactly once (marked `DisplayAttached`). The images are filled by the
/// render-graph node each frame (in place, stable textures), so the sprites show
/// the live animations. At N=1 this is the M1a display, centered.
fn attach_display(
    mut commands: Commands,
    cfg: Res<Cfg>,
    query: Query<(Entity, &RiveTarget, &RiveSlot), Without<DisplayAttached>>,
) {
    for (entity, target, slot) in &query {
        if target.image == Handle::default() {
            continue; // image not allocated yet
        }
        commands.spawn((
            Sprite::from_image(target.image.clone()),
            grid_transform(slot.0, cfg.instances, cfg.size as f32),
            DisplayQuad,
        ));
        commands.entity(entity).insert(DisplayAttached);
    }
}

/// Lays instance `index` of `total` in a centered grid, scaling each cell so the
/// whole grid fits a nominal 1200×680 viewport. Cosmetic only — the perf cost is
/// the rive fill, not the sprite size; at N=1 the scale is 1.0 (identity display).
fn grid_transform(index: u32, total: u32, cell_px: f32) -> Transform {
    let cols = (total as f32).sqrt().ceil().max(1.0);
    let rows = ((total as f32) / cols).ceil().max(1.0);
    let cell = (1200.0_f32 / cols).min(680.0_f32 / rows);
    let scale = if total > 1 { cell / cell_px } else { 1.0 };
    let col = index as f32 % cols;
    let row = (index as f32 / cols).floor();
    // Center the grid on the origin (the 2D camera sits at 0,0); +y is up.
    let x = (col - (cols - 1.0) / 2.0) * cell;
    let y = -(row - (rows - 1.0) / 2.0) * cell;
    Transform::from_translation(Vec3::new(x, y, 0.0)).with_scale(Vec3::splat(scale))
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
    // Capture is a single-window screenshot (correctness runs use N=1); with N>1
    // it still captures the whole grid. Use the first instance to gate readiness.
    let Some(target) = query.iter().next() else {
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

/// M2.0 perf mode: count frames and exit once the budget is reached, so a
/// `RIVE_PERF` run self-terminates after the render-world collector has logged its
/// summary. No-op unless `RIVE_PERF` was set (`perf_exit_frames` is `None`).
fn drive_perf_exit(cfg: Res<Cfg>, mut frames: Local<u32>, mut exit: MessageWriter<AppExit>) {
    let Some(budget) = cfg.perf_exit_frames else {
        return;
    };
    *frames += 1;
    if *frames >= budget {
        info!("rive: perf budget ({budget} frames) reached, exiting");
        exit.write(AppExit::Success);
    }
}
