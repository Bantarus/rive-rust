//! Milestone 1b deliverable: a `.riv` animates on a sprite, driven each frame by
//! the native Rive Renderer rendering **directly into a wgpu-allocated `VkImage`**
//! (zero copy — no per-frame CPU readback), via the `bevy-rive` `zero_copy` tier.
//!
//! Build/run requires the `zero_copy` feature **and native Vulkan** (set
//! `WGPU_BACKEND=vulkan`). PLS path is capability-gated (M2c): raster-order
//! (`VK_EXT_rasterization_order_attachment_access`) if present, else clockwise where
//! pixel interlock (`VK_EXT_fragment_shader_interlock`, e.g. NVIDIA desktop) is
//! available, else the atomic fallback (all correct).
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
//!   RIVE_NO_CLOCKWISE=1           force the atomic PLS path (M2c default is clockwise
//!                                 wherever pixel interlock is available); RIVE_CLOCKWISE=1
//!                                 forces clockwise; RIVE_FORCE_ATOMIC=1 suppresses interlock.
//!   RIVE_VM_SET_ENUM="path=index" attach a RiveViewModel to each face and re-assert this
//!                                 enum write every frame — proves view-model WRITE forwarding
//!                                 in the render-world (zero-copy) tier. Pair with a face that
//!                                 has a visible enum, e.g.
//!                                 RIVE_RIV=voxelien_face.riv RIVE_VM_SET_ENUM="viseme=8"
//!                                 (mouth shape changes vs index 0).
//!   RIVE_VM_WATCH="path"          watch this NUMBER property on each face and log its
//!                                 read-back — proves view-model watch READ-BACK through the
//!                                 render→main channel (M-READBACK). A script-driven value
//!                                 shows live movement, e.g.
//!                                 RIVE_RIV=voxelien_face.riv RIVE_VM_WATCH="breath/scaleX".
//!   RIVE_VM_OBSERVE="p1,p2"       observe these paths — each change/trigger-fire emits a
//!                                 RivePropertyChanged message, counted + logged (proves
//!                                 OBSERVE through the same channel). A script-driven path
//!                                 fires ~every frame; a static one never, e.g.
//!                                 RIVE_RIV=voxelien_face.riv RIVE_VM_OBSERVE="breath/scaleX,viseme".
//!   RIVE_RIG_WATCH="name"         watch bone `name`'s rotation + constraint `name`'s strength
//!                                 (empty name = the first unnamed component of each type) and
//!                                 log the read-backs — proves RIG read-back through the channel.
//!                                 An animation-driven bone shows live movement, e.g.
//!                                 RIVE_RIV=9939-18941-big-wheel-demo.riv RIVE_RIG_WATCH="".
//!   RIVE_RIG_SPIN=1               drive the watched bone's rotation procedurally (+3°/frame,
//!                                 via the proven RiveRig write ferry) so the rig read-back has
//!                                 a MOVING value to track — proves the write→advance→read
//!                                 round-trip (the read follows one frame behind). Pair with
//!                                 RIVE_RIG_WATCH.
//!   RIVE_WATCH_PLAYHEAD=1         watch the live playhead + duration and log them — proves
//!                                 PLAYHEAD read-back. Only a linear-animation scene has a
//!                                 scalar playhead, e.g. RIVE_RIV=5122-10308-eye-joysticks-demo.riv
//!                                 (a 3s loop; on a state machine both read None).
//!   RIVE_WATCH_FOCUS=1            watch the state machine's FocusState and log it — proves
//!                                 FOCUS read-back (API-level: no demo asset authors FocusData,
//!                                 so the delivered state is the default nothing-focused).
//!   RIVE_TEXT_WATCH="name"        watch text run `name` (empty = first unnamed) and log its
//!                                 read-back string — proves TEXT read-back through the channel
//!                                 (one frame behind). E.g. RIVE_RIV=9939-18941-big-wheel-demo.riv
//!                                 RIVE_TEXT_WATCH="" reads "BIG".
//!   RIVE_TEXT_SET="value"         set the watched run to `value` at spawn (via the proven
//!                                 RiveText write ferry) so the read tracks the write. Pair with
//!                                 RIVE_TEXT_WATCH.
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
use bevy::window::PresentMode;
use bevy::winit::WinitSettings;

use bevy_rive::{
    install_interlock_device_callback, BoneProp, NewViewModel, RiveActive, RiveAnimation,
    RiveAtlasKey, RiveFile, RiveInput, RivePointer, RivePropertyChanged, RiveRig, RiveSampling,
    RiveSurface, RiveTarget, RiveText, RiveViewModel, RiveZeroCopyPlugin,
};

/// A one-shot structural list command parsed from `RIVE_VM_LIST` (see `Cfg::vm_list`).
#[derive(Clone)]
enum VmListOp {
    Clear,
    Swap(usize, usize),
    Remove(usize),
    Add(String),
}

/// Parses `RIVE_VM_LIST="op;path[;args]"` into `(path, op)`. Returns `None` (with a
/// warning) on a malformed spec.
fn parse_vm_list(spec: &str) -> Option<(String, VmListOp)> {
    let parts: Vec<&str> = spec.split(';').collect();
    let op = match parts.as_slice() {
        ["clear", path] => (path, VmListOp::Clear),
        ["remove", path, i] => (path, VmListOp::Remove(i.trim().parse().ok()?)),
        ["add", path, vm] => (path, VmListOp::Add((*vm).to_string())),
        ["swap", path, ab] => {
            let (a, b) = ab.split_once(',')?;
            (path, VmListOp::Swap(a.trim().parse().ok()?, b.trim().parse().ok()?))
        }
        _ => {
            warn!("RIVE_VM_LIST: bad spec {spec:?} (want op;path[;args])");
            return None;
        }
    };
    Some((op.0.trim().to_string(), op.1))
}

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
    /// M-SCALE: when `RIVE_ATLAS` is set, opt every face into the shared atlas (one
    /// render pass for all of them); each sprite then samples its tile via `RiveSurface`.
    atlas: bool,
    /// M-SCALE Phase 3: when `RIVE_CULL` is set, oscillate `RiveActive` on the odd-slot
    /// faces (deactivate → reactivate) — exercises the per-LOD packer's free/realloc and
    /// the `Changed`/`Removed` `RiveSurface` re-sync. Off by default (perf/capture runs
    /// stay deterministic and full-grid).
    cull: bool,
    /// M-SCALE Phase 4: number of `RiveAtlasKey` pools to round-robin atlas faces across
    /// (`RIVE_KEYS`, default 1). >1 exercises key partitioning (distinct keys ⇒ distinct pages).
    keys: u32,
    /// M-DATA: `RIVE_VM_SET_ENUM="path=index"` — attach a `RiveViewModel` to each face and
    /// re-assert this enum write every frame, proving view-model WRITE forwarding through the
    /// render-world (zero-copy) advance path. `None` (no VM write) by default.
    vm_set_enum: Option<(String, u32)>,
    /// M-DATA: `RIVE_VM_ONESHOT=1` — queue the `vm_set_enum` write ONCE at spawn (before the
    /// `.riv` finishes loading) and never re-assert it. Proves the write is RETAINED across the
    /// async-load window and applied once the face goes live (the lost-update fix), vs the
    /// default every-frame re-assert which self-heals.
    vm_oneshot: bool,
    /// M-READBACK: `RIVE_VM_WATCH="path"` — watch this NUMBER property on each face and log
    /// its per-frame read-back, proving watch READ-BACK through the render→main channel.
    vm_watch: Option<String>,
    /// M-READBACK: `RIVE_VM_OBSERVE="p1,p2"` — observe these paths; `report_vm_readback`
    /// counts + logs the resulting `RivePropertyChanged` messages.
    vm_observe: Vec<String>,
    /// M-LIST: `RIVE_VM_LIST="op;path[;args]"` — queue ONE structural list command at
    /// spawn (ferried through the render-world advance path, proving the zero-copy
    /// list ferry). `op` ∈ clear / swap (args `a,b`) / remove (arg `i`) / add (arg
    /// `VmName`, constructs a blank instance + appends). `None` by default. E.g.
    /// `RIVE_RIV=25759-48234-slot-machine-game-with-scripting.riv`
    /// `RIVE_VM_LIST="clear;Chicken/WheelCenter/WheelList"`.
    vm_list: Option<(String, VmListOp)>,
    /// M-READBACK: `RIVE_RIG_WATCH="name"` — watch bone `name` rotation + constraint
    /// `name` strength on each face and log the read-backs (`""` = first unnamed).
    rig_watch: Option<String>,
    /// M-READBACK: `RIVE_RIG_SPIN=1` — drive the watched bone's rotation (+3°/frame)
    /// so the read-back tracks a moving value (the write→advance→read round-trip).
    rig_spin: bool,
    /// M-READBACK: `RIVE_WATCH_PLAYHEAD=1` — watch the live playhead/duration.
    watch_playhead: bool,
    /// M-READBACK: `RIVE_WATCH_FOCUS=1` — watch the state machine's `FocusState`.
    watch_focus: bool,
    /// M-READBACK: `RIVE_TEXT_WATCH="name"` — watch text run `name` ("" = first unnamed).
    text_watch: Option<String>,
    /// M-READBACK: `RIVE_TEXT_SET="value"` — set the watched run at spawn (read tracks write).
    text_set: Option<String>,
    /// M-INPUT: `RIVE_POINTER="x,y"` — attach a `RivePointer` at this fixed TARGET-PIXEL
    /// position to each face, proving pointer-input forwarding through the zero-copy
    /// (render-world) advance path. The node re-asserts `pointer_move` every frame, so a
    /// pointer-driven joystick (e.g. nimai's gaze) eases toward it. Pair with a listener face:
    /// `RIVE_RIV=nimai_published.riv RIVE_POINTER="80,80"` vs `"432,432"` → eyes/head track
    /// distinct directions. `None` (no pointer) by default.
    pointer: Option<Vec2>,
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

/// Links a rive (face) entity to its display-sprite entity (the two-entity pattern), so the
/// `Changed`/`Removed` `RiveSurface` re-sync can find the sprite to re-point at a new tile
/// (after a cull/LOD repack) or hide (after a cull frees the tile).
#[derive(Component)]
struct DisplayLink(Entity);

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
    // M-SCALE Phase 3: per-face target size — picks the atlas LOD bucket under RIVE_ATLAS
    // (≤128 → 128-bucket, ≤256 → 256, else 512). Default 512 (the M1a/M2 target size).
    let size = std::env::var("RIVE_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(512u32)
        .clamp(16, 2048);
    // M-SCALE Phase 4: split atlas faces across this many RiveAtlasKey pools (round-robin
    // by index) to exercise key partitioning — distinct keys never share a page. Default 1.
    let keys = std::env::var("RIVE_KEYS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1u32)
        .clamp(1, 16);
    // M-DATA: RIVE_VM_SET_ENUM="path=index" — drive a view-model enum every frame to
    // prove WRITE forwarding through the zero-copy (render-world) advance path.
    let vm_set_enum = std::env::var("RIVE_VM_SET_ENUM").ok().and_then(|s| {
        let (path, idx) = s.split_once('=')?;
        Some((path.trim().to_string(), idx.trim().parse().ok()?))
    });
    // M-READBACK: RIVE_VM_WATCH="path" (a number property) + RIVE_VM_OBSERVE="p1,p2" —
    // prove watch read-back + change/trigger observation through the render→main channel.
    let vm_watch = std::env::var("RIVE_VM_WATCH")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let vm_observe: Vec<String> = std::env::var("RIVE_VM_OBSERVE")
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    // M-READBACK: rig / playhead / focus read-back knobs (see the header). The rig
    // name may be EMPTY (= the first unnamed bone/constraint), so `.ok()` alone —
    // no emptiness filter like vm_watch's.
    let rig_watch = std::env::var("RIVE_RIG_WATCH")
        .ok()
        .map(|s| s.trim().to_string());
    let rig_spin = std::env::var_os("RIVE_RIG_SPIN").is_some();
    let watch_playhead = std::env::var_os("RIVE_WATCH_PLAYHEAD").is_some();
    let watch_focus = std::env::var_os("RIVE_WATCH_FOCUS").is_some();
    // M-READBACK: RIVE_TEXT_WATCH="name" watches a text run; RIVE_TEXT_SET drives it.
    let text_watch = std::env::var("RIVE_TEXT_WATCH")
        .ok()
        .map(|s| s.trim().to_string());
    // Not trimmed — the set value is used verbatim (may contain spaces).
    let text_set = std::env::var("RIVE_TEXT_SET").ok();
    // M-INPUT: RIVE_POINTER="x,y" — a fixed target-pixel pointer position forwarded to each
    // face, proving pointer-input forwarding through the zero-copy advance path.
    let pointer = std::env::var("RIVE_POINTER").ok().and_then(|s| {
        let (x, y) = s.split_once(',')?;
        Some(Vec2::new(x.trim().parse().ok()?, y.trim().parse().ok()?))
    });
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

    // M2b correctness knob: select the swapchain present mode to exercise the GPU
    // completion watermark under CPU run-ahead. Default Fifo (vsync) matches the M2a
    // measurements; Immediate / Mailbox let the CPU outrun the GPU past rive's ring,
    // which only the M2b timeline-semaphore watermark (not the fixed offset) handles.
    let present_mode = match std::env::var("RIVE_PRESENT_MODE").ok().as_deref() {
        Some("immediate") => PresentMode::Immediate,
        Some("mailbox") => PresentMode::Mailbox,
        Some("fifo_relaxed") => PresentMode::FifoRelaxed,
        _ => PresentMode::Fifo,
    };

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
            // M2b: drive the primary window's present mode from RIVE_PRESENT_MODE so we
            // can validate the watermark under non-Fifo (CPU-run-ahead) configurations.
            .set(WindowPlugin {
                primary_window: Some(Window {
                    present_mode,
                    ..default()
                }),
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
    // `::default()` anchors the fill in BOTH the Core2d + Core3d graphs; this is a 2D
    // `Sprite` scene, so the Core2d node is the one that runs. A pure-3D consumer
    // (Camera3d, no Camera2d) would use `RiveZeroCopyPlugin::anchored(RiveGraphAnchor::Core3d)`.
    .add_plugins(RiveZeroCopyPlugin::default())
    .insert_resource(WinitSettings::continuous())
    .insert_resource(Cfg {
        riv,
        size,
        capture,
        warmup,
        speed,
        instances,
        perf_exit_frames,
        atlas: std::env::var_os("RIVE_ATLAS").is_some(),
        cull: std::env::var_os("RIVE_CULL").is_some(),
        keys,
        vm_set_enum,
        vm_oneshot: std::env::var_os("RIVE_VM_ONESHOT").is_some(),
        vm_watch,
        vm_observe,
        vm_list: std::env::var("RIVE_VM_LIST")
            .ok()
            .and_then(|s| parse_vm_list(s.trim())),
        rig_watch,
        rig_spin,
        watch_playhead,
        watch_focus,
        text_watch,
        text_set,
        pointer,
    })
    .init_resource::<CaptureState>()
    .add_systems(Startup, setup)
    .add_systems(
        Update,
        (
            attach_display,
            resync_atlas_sprites,
            drive_cull,
            drive_vm_writes,
            drive_rig_spin,
            report_vm_readback,
            report_reads,
            drive_capture,
            drive_perf_exit,
        )
            .chain(),
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
        // M-READBACK: register the live playhead/duration read (RIVE_WATCH_PLAYHEAD).
        if cfg.watch_playhead {
            anim.watch_playhead();
        }
        // M-SCALE: under RIVE_ATLAS, opt every face into the shared atlas (one pass for
        // all of them); otherwise each gets its own dedicated image (the default tier).
        // RIVE_KEYS>1 round-robins faces across that many pools (distinct keys ⇒ distinct pages).
        let target = if cfg.atlas {
            RiveTarget::atlased(cfg.size, cfg.size, RiveAtlasKey(i % cfg.keys))
        } else {
            RiveTarget::new(cfg.size, cfg.size)
        };
        let mut e = commands.spawn((anim, target, RiveEntity, RiveSlot(i)));
        // Under RIVE_CULL the driver toggles RiveActive; add it now so the component
        // exists to mutate (absent would mean "always active" — nothing to cull).
        if cfg.cull {
            e.insert(RiveActive(true));
        }
        // M-DATA: attach a RiveViewModel so `drive_vm_writes` can queue an enum write
        // (forwarded to the render world and applied before advance). Under RIVE_VM_ONESHOT,
        // queue the write ONCE here at spawn (before the .riv loads) and never re-assert it —
        // the write must be retained across the load window to land.
        // M-READBACK: the same component carries the watch/observe registrations —
        // the render node reads/flushes them after advance and the results come back
        // over the render→main channel (RIVE_VM_WATCH / RIVE_VM_OBSERVE).
        if cfg.vm_set_enum.is_some()
            || cfg.vm_watch.is_some()
            || !cfg.vm_observe.is_empty()
            || cfg.vm_list.is_some()
        {
            let mut vm = RiveViewModel::default();
            if cfg.vm_oneshot {
                if let Some((path, index)) = &cfg.vm_set_enum {
                    vm.set_enum_index(path.clone(), *index);
                }
            }
            if let Some(path) = &cfg.vm_watch {
                vm.watch_number(path.clone());
            }
            for path in &cfg.vm_observe {
                vm.observe(path.clone());
            }
            // M-LIST: queue the one-shot structural command (ferried through the
            // zero-copy advance path; retained across the async-load window like the
            // one-shot enum write above, so it lands once the face goes live).
            if let Some((path, op)) = &cfg.vm_list {
                match op {
                    VmListOp::Clear => vm.list_clear(path.clone()),
                    VmListOp::Swap(a, b) => vm.list_swap(path.clone(), *a, *b),
                    VmListOp::Remove(i) => vm.list_remove_at(path.clone(), *i),
                    VmListOp::Add(name) => {
                        vm.list_add_new(path.clone(), NewViewModel::blank(name.clone()));
                    }
                }
            }
            e.insert(vm);
        }
        // M-INPUT: attach a fixed-position pointer so the zero-copy node forwards it to the
        // state machine's Listeners each frame (a joystick eases toward it). Set once here;
        // the every-frame re-assert lives in the node (extract re-reads this component).
        if let Some(pos) = cfg.pointer {
            e.insert(RivePointer {
                pos: Some(pos),
                primary_down: false,
            });
        }
        // M-READBACK: register rig reads (bone rotation + constraint strength by the
        // same name; "" = first unnamed) and the focus read — `report_reads` logs
        // the values the render→main channel delivers.
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
        // M-READBACK: register a text-run read (and optionally drive it) — `report_reads`
        // logs the string the render→main channel delivers (one frame behind).
        if let Some(name) = &cfg.text_watch {
            let mut text = RiveText::default();
            text.watch_text(name.clone());
            if let Some(value) = &cfg.text_set {
                text.set(name.clone(), value.clone());
            }
            e.insert(text);
        }
    }
}

/// M-DATA: re-assert the configured view-model enum write on every face each frame
/// (a held value — robust even if the state machine re-evaluates it), proving WRITE
/// forwarding through the zero-copy advance path. No-op unless `RIVE_VM_SET_ENUM` was set.
fn drive_vm_writes(cfg: Res<Cfg>, mut q: Query<&mut RiveViewModel>) {
    let Some((path, index)) = cfg.vm_set_enum.as_ref() else {
        return;
    };
    // One-shot mode queues the write once at spawn (in `setup`); do NOT re-assert here.
    if cfg.vm_oneshot {
        return;
    }
    for mut vm in &mut q {
        vm.set_enum_index(path.clone(), *index);
    }
}

/// M-READBACK: the consumer-side proof that view-model reads flow render→main
/// under `zero_copy`. Logs the watched number's read-back (`RiveViewModel::values`,
/// filled by the channel drain) whenever it changes — first 10 changes verbosely,
/// then just the tally — and counts `RivePropertyChanged` fires per observed path,
/// printing a cumulative tally every 60 frames. No-op unless `RIVE_VM_WATCH` /
/// `RIVE_VM_OBSERVE` was set.
fn report_vm_readback(
    cfg: Res<Cfg>,
    q: Query<(Entity, &RiveViewModel)>,
    mut fires: MessageReader<RivePropertyChanged>,
    mut fire_tally: Local<std::collections::HashMap<String, u32>>,
    mut last: Local<std::collections::HashMap<Entity, f32>>,
    mut value_changes: Local<u32>,
    mut frames: Local<u32>,
) {
    if cfg.vm_watch.is_none() && cfg.vm_observe.is_empty() {
        return;
    }
    *frames += 1;
    for msg in fires.read() {
        *fire_tally.entry(msg.path.clone()).or_insert(0) += 1;
    }
    if let Some(path) = &cfg.vm_watch {
        for (entity, vm) in &q {
            if let Some(v) = vm.number(path) {
                if last.get(&entity) != Some(&v) {
                    *value_changes += 1;
                    if *value_changes <= 10 {
                        info!("vm watch read-back: {path:?} = {v} ({entity:?}, change #{})", *value_changes);
                    }
                    last.insert(entity, v);
                }
            }
        }
    }
    if frames.is_multiple_of(60) {
        let fires: Vec<String> = cfg
            .vm_observe
            .iter()
            .map(|p| format!("{p:?}={}", fire_tally.get(p).copied().unwrap_or(0)))
            .collect();
        info!(
            "vm read-back tally after {} frames: watch changes={}, observe fires: [{}]",
            *frames,
            *value_changes,
            fires.join(", ")
        );
    }
}

/// M-READBACK proof driver: spin the watched bone (+3°/frame via the proven
/// `RiveRig` write ferry) so the rig read-back has a MOVING value to track —
/// the write→advance→read round-trip. No-op unless `RIVE_RIG_SPIN` (and
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

/// M-READBACK: the consumer-side proof for the rig / playhead / focus reads —
/// logs each read-back's value when it changes (bone rotation verbosely for the
/// first 10 changes) and a cumulative tally every 60 frames. No-op unless
/// `RIVE_RIG_WATCH` / `RIVE_WATCH_PLAYHEAD` / `RIVE_WATCH_FOCUS` was set.
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
    // With N>1 instances, the `*_now` snapshots report the last-iterated face.
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
    // Once per FRAME (not per entity), so the count stays a frame tally at N>1.
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

/// Spawns a display sprite for each rive instance once the plugin has allocated
/// its target image — laid out in a grid (M2a multi-instance). Each instance is
/// attached exactly once (marked `DisplayAttached`). The images are filled by the
/// render-graph node each frame (in place, stable textures), so the sprites show
/// the live animations. At N=1 this is the M1a display, centered.
fn attach_display(
    mut commands: Commands,
    cfg: Res<Cfg>,
    query: Query<(Entity, &RiveTarget, &RiveSlot, Option<&RiveSurface>), Without<DisplayAttached>>,
) {
    for (entity, target, slot, surface) in &query {
        // Atlas faces sample their TILE of the shared atlas (via `RiveSampling`); dedicated
        // faces sample their whole per-face image (the default tier).
        let sprite = if target.atlas.is_some() {
            let Some(surface) = surface else {
                continue; // slot not assigned / RiveSurface not written yet
            };
            let mut s = Sprite::from_image(surface.image.clone());
            s.rect = Some(RiveSampling::sprite_rect(surface)); // this face's tile sub-rect
            s
        } else {
            if target.image == Handle::default() {
                continue; // image not allocated yet
            }
            Sprite::from_image(target.image.clone())
        };
        let display = commands
            .spawn((
                sprite,
                grid_transform(slot.0, cfg.instances, cfg.size as f32),
                DisplayQuad,
                Visibility::Visible,
            ))
            .id();
        // Mark the face attached (one-shot spawn) and link it to its sprite so the re-sync
        // can re-point/hide that sprite after a later cull or LOD repack.
        commands
            .entity(entity)
            .insert((DisplayAttached, DisplayLink(display)));
    }
}

/// Re-syncs each atlas face's display sprite to its CURRENT tile (M-SCALE Phase 3). The
/// main-world packer can hand a face a NEW `uv_rect` after a cull/LOD repack, or REMOVE
/// `RiveSurface` when the face is culled (its tile is then freed for another face) — a
/// one-shot latch would strand a stale sub-rect, so an atlas consumer MUST run this:
///  * `Changed<RiveSurface>` → re-point the sprite at the new tile and show it.
///  * `RemovedComponents<RiveSurface>` → hide the sprite (its old tile may now be reused).
///
/// (The first-frame insert is handled by `attach_display`, which spawns the sprite + link;
/// this system covers every change AFTER the link exists.)
fn resync_atlas_sprites(
    changed: Query<(&RiveSurface, &DisplayLink), Changed<RiveSurface>>,
    mut removed: RemovedComponents<RiveSurface>,
    links: Query<&DisplayLink>,
    mut sprites: Query<(&mut Sprite, &mut Visibility), With<DisplayQuad>>,
) {
    for (surface, link) in &changed {
        if let Ok((mut sprite, mut vis)) = sprites.get_mut(link.0) {
            sprite.image = surface.image.clone();
            sprite.rect = Some(RiveSampling::sprite_rect(surface));
            *vis = Visibility::Visible;
        }
    }
    for face in removed.read() {
        if let Ok(link) = links.get(face) {
            if let Ok((_, mut vis)) = sprites.get_mut(link.0) {
                *vis = Visibility::Hidden;
            }
        }
    }
}

/// `RIVE_CULL` exercise (M-SCALE Phase 3): oscillate the odd-slot faces' [`RiveActive`] so
/// the per-LOD packer frees + reallocates their tiles and `resync_atlas_sprites` re-points
/// or hides their sprites. No-op unless `RIVE_CULL` was set (perf/capture runs are full-grid).
fn drive_cull(cfg: Res<Cfg>, mut frame: Local<u32>, mut q: Query<(&RiveSlot, &mut RiveActive)>) {
    if !cfg.cull {
        return;
    }
    *frame += 1;
    // Flip the odd slots active/inactive every 90 frames; even slots stay active.
    let odd_active = (*frame / 90).is_multiple_of(2);
    for (slot, mut active) in &mut q {
        let want = slot.0.is_multiple_of(2) || odd_active;
        if active.0 != want {
            active.0 = want;
        }
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
    // Atlas faces have no per-face image; gate on the display quad existing (it is
    // spawned once the atlas slot / RiveSurface is ready). Dedicated faces gate on
    // their image being allocated.
    let img_ready = target.atlas.is_some() || target.image != Handle::default();
    if !img_ready || quad.is_empty() {
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
