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
//!   RIVE_RIG_WATCH="name"              M-READBACK floor proof: watch bone `name`'s
//!                                      rotation + constraint `name`'s strength ("" =
//!                                      first unnamed of each type) and log read-backs
//!   RIVE_RIG_SPIN=1                    drive the watched bone (+3°/frame) so the
//!                                      read-back tracks a moving value (write→read
//!                                      round-trip; pair with RIVE_RIG_WATCH)
//!   RIVE_WATCH_PLAYHEAD=1              watch + log the live playhead/duration (only a
//!                                      linear-animation scene has one)
//!   RIVE_WATCH_FOCUS=1                 watch + log the state machine's FocusState
//!   RIVE_TEXT_WATCH="name"             M-READBACK text proof: watch text run `name`
//!                                      ("" = first unnamed) and log its read-back string
//!   RIVE_TEXT_SET="value"              set the watched run to `value` at spawn (via the
//!                                      proven RiveText write path) so the read tracks the
//!                                      write; pair with RIVE_TEXT_WATCH
//!
//! The frozen color/orientation contract requires `Tonemapping::None` + no `Hdr`
//! on the camera (so the sRGB sample->output round-trip is an identity) and
//! `Msaa::Off` for a clean pixel comparison against the M0/M1.0 PNG references.

use bevy::core_pipeline::tonemapping::{DebandDither, Tonemapping};
use bevy::prelude::*;
use bevy::render::view::screenshot::{save_to_disk, Screenshot};
use bevy::winit::WinitSettings;

use bevy_rive::{
    BoneProp, RiveAnimation, RiveFile, RiveInput, RivePlugin, RiveRig, RiveTarget, RiveText,
};

#[derive(Resource)]
struct Cfg {
    riv: String,
    size: u32,
    capture: Option<String>,
    warmup: u32,
    speed: f32,
    /// M-READBACK floor proof: watch bone/constraint `name` ("" = first unnamed).
    rig_watch: Option<String>,
    /// M-READBACK floor proof: drive the watched bone (+3°/frame) so the read-back
    /// tracks a moving value.
    rig_spin: bool,
    /// M-READBACK floor proof: watch the live playhead/duration.
    watch_playhead: bool,
    /// M-READBACK floor proof: watch the state machine's `FocusState`.
    watch_focus: bool,
    /// M-READBACK text proof: watch text run `name` ("" = first unnamed).
    text_watch: Option<String>,
    /// M-READBACK text proof: set the watched run to this value at spawn, so the
    /// read-back tracks a write (the text analogue of `rig_spin`).
    text_set: Option<String>,
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
            rig_watch: std::env::var("RIVE_RIG_WATCH")
                .ok()
                .map(|s| s.trim().to_string()),
            rig_spin: std::env::var_os("RIVE_RIG_SPIN").is_some(),
            watch_playhead: std::env::var_os("RIVE_WATCH_PLAYHEAD").is_some(),
            watch_focus: std::env::var_os("RIVE_WATCH_FOCUS").is_some(),
            text_watch: std::env::var("RIVE_TEXT_WATCH")
                .ok()
                .map(|s| s.trim().to_string()),
            // Not trimmed — the set value is used verbatim (may contain spaces).
            text_set: std::env::var("RIVE_TEXT_SET").ok(),
        })
        .init_resource::<CaptureState>()
        .add_systems(Startup, setup)
        .add_systems(
            Update,
            (attach_display, drive_rig_spin, report_reads, drive_capture).chain(),
        )
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
    // M-READBACK floor proof: register the opt-in reads (see the header knobs);
    // `report_reads` logs what the floor advance system writes back inline.
    if cfg.watch_playhead {
        anim.watch_playhead();
    }
    let mut e = commands.spawn((anim, RiveTarget::new(cfg.size, cfg.size), RiveEntity));
    if let Some(name) = &cfg.rig_watch {
        let mut rig = RiveRig::default();
        rig.watch_bone(name.clone(), BoneProp::Rotation);
        rig.watch_constraint_strength(name.clone());
        e.insert(rig);
    }
    if cfg.watch_focus {
        let mut input = RiveInput::default();
        input.watch_focus();
        e.insert(input);
    }
    if let Some(name) = &cfg.text_watch {
        let mut text = RiveText::default();
        text.watch_text(name.clone());
        // Optionally drive the run so the read-back tracks a write (like RIVE_RIG_SPIN
        // for bones) — set BEFORE the first advance so the read returns it this tick.
        if let Some(value) = &cfg.text_set {
            text.set(name.clone(), value.clone());
        }
        e.insert(text);
    }
    // `attach_display` spawns the textured quad once the plugin writes the real
    // image handle back into RiveTarget.
}

/// M-READBACK proof driver: spin the watched bone (+3°/frame via the proven
/// `RiveRig` write path) so the read-back tracks a moving value — the
/// write→advance→read round-trip. No-op unless `RIVE_RIG_SPIN` (and
/// `RIVE_RIG_WATCH`) was set.
fn drive_rig_spin(cfg: Res<Cfg>, mut q: Query<&mut RiveRig>, mut angle: Local<f32>) {
    if !cfg.rig_spin {
        return;
    }
    let Some(name) = cfg.rig_watch.as_ref() else {
        return;
    };
    *angle = (*angle + 3.0) % 360.0;
    for mut rig in &mut q {
        rig.set_bone(name.clone(), BoneProp::Rotation, *angle);
    }
}

/// M-READBACK floor proof: logs each watched read-back when it changes (bone
/// rotation verbosely for the first 10 changes) + a tally every 60 frames — the
/// floor analogue of the zero-copy example's `report_reads`. No-op unless a
/// read knob was set.
// A Bevy system: the arg count and the multi-component query tuple are inherent to
// what it observes, so the two clippy heuristics don't apply.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn report_reads(
    cfg: Res<Cfg>,
    q: Query<(
        Entity,
        &RiveAnimation,
        Option<&RiveRig>,
        Option<&RiveInput>,
        Option<&RiveText>,
    )>,
    mut last_bone: Local<std::collections::HashMap<Entity, f32>>,
    mut bone_changes: Local<u32>,
    mut last_text: Local<std::collections::HashMap<Entity, Option<String>>>,
    mut text_changes: Local<u32>,
    mut playhead_frames: Local<u32>,
    mut frames: Local<u32>,
) {
    if cfg.rig_watch.is_none()
        && !cfg.watch_playhead
        && !cfg.watch_focus
        && cfg.text_watch.is_none()
    {
        return;
    }
    *frames += 1;
    let mut playhead_now = None;
    let mut strength_now = None;
    let mut focus_now = None;
    let mut text_now = None;
    let mut any_playhead = false;
    for (entity, anim, rig, input, text) in &q {
        if let (Some(name), Some(rig)) = (cfg.rig_watch.as_ref(), rig) {
            if let Some(rot) = rig.bone(name, BoneProp::Rotation) {
                if last_bone.get(&entity) != Some(&rot) {
                    *bone_changes += 1;
                    if *bone_changes <= 10 {
                        info!(
                            "rig read-back: bone {name:?} rotation = {rot} ({entity:?}, change #{})",
                            *bone_changes
                        );
                    }
                    last_bone.insert(entity, rot);
                }
            }
            strength_now = rig.constraint_strength(name);
        }
        if let (Some(name), Some(text)) = (cfg.text_watch.as_ref(), text) {
            let now = text.text(name).map(|s| s.to_string());
            if last_text.get(&entity) != Some(&now) {
                *text_changes += 1;
                if *text_changes <= 10 {
                    info!(
                        "text read-back: run {name:?} = {now:?} ({entity:?}, change #{})",
                        *text_changes
                    );
                }
                last_text.insert(entity, now.clone());
            }
            text_now = now;
        }
        if cfg.watch_playhead {
            any_playhead |= anim.playhead().is_some();
            playhead_now = Some((anim.playhead(), anim.duration()));
        }
        if let Some(input) = input {
            focus_now = Some(input.focus_state());
        }
    }
    // Once per FRAME (not per entity), so the count stays a frame tally.
    if any_playhead {
        *playhead_frames += 1;
    }
    if frames.is_multiple_of(60) {
        info!(
            "read-back tally after {} frames: bone changes={}, constraint strength={:?}, \
             playhead frames-with-value={}, playhead now={:?}, focus={:?}, \
             text changes={}, text now={:?}",
            *frames,
            *bone_changes,
            strength_now,
            *playhead_frames,
            playhead_now,
            focus_now,
            *text_changes,
            text_now,
        );
    }
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
