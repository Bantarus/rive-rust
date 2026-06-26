//! Milestone 0 deliverable: render a `.riv` file's default state machine to an
//! offscreen image with the native Rive Renderer and write it to a PNG.
//!
//! Usage:
//!   cargo run --example offscreen_png -- [INPUT.riv] [OUTPUT.png] [WIDTH] [HEIGHT]
//!
//! Defaults: assets/coffee_loader.riv, out.png, 512x512.

use std::io::BufWriter;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use rive_renderer::{unpremultiply_rgba8, Context};

// Opaque dark gray, matching rive's own path_fiddle demo (0xff303030). An
// opaque clear makes premultiplied == straight alpha, so the PNG is correct
// without any color surgery; un-premultiply below is then a no-op.
const CLEAR_RGBA: [f32; 4] = [0.188, 0.188, 0.188, 1.0];

// ~16 ms: advance the state machine by a single 60 Hz frame.
const FRAME_DT_SECONDS: f32 = 1.0 / 60.0;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let input = args
        .next()
        .map_or_else(|| PathBuf::from("assets/coffee_loader.riv"), PathBuf::from);
    let output = args
        .next()
        .map_or_else(|| PathBuf::from("out.png"), PathBuf::from);
    let width: u32 = parse_or(args.next(), 512, "WIDTH")?;
    let height: u32 = parse_or(args.next(), 512, "HEIGHT")?;

    let riv_bytes = std::fs::read(&input)
        .with_context(|| format!("reading .riv file {}", input.display()))?;

    println!(
        "rendering {} ({}x{}) -> {}",
        input.display(),
        width,
        height,
        output.display()
    );

    // Bring up the native Rive Renderer on a self-managed headless Vulkan device.
    let ctx = Context::new().context("creating the Rive Vulkan context")?;
    let target = ctx
        .offscreen_target(width, height)
        .context("creating the offscreen render target")?;

    // Load content and grab an artboard + state machine (honoring selection knobs).
    // RIVE_ASSETS: route the load through the out-of-band asset loader, logging
    // every referenced asset (name / kind / extension / in-band size). Supplies
    // bytes from a directory of files named "<asset name>" when RIVE_ASSET_DIR is
    // set; otherwise declines (→ in-band fallback). Proves the loader callback.
    let file = if std::env::var("RIVE_ASSETS").is_ok() {
        let dir = std::env::var("RIVE_ASSET_DIR").ok();
        ctx.load_file_with_assets(&riv_bytes, |req| {
            let supplied = dir
                .as_deref()
                .and_then(|d| std::fs::read(std::path::Path::new(d).join(req.name)).ok());
            let action = match &supplied {
                Some(b) => format!("supply {}B", b.len()),
                None => "decline (in-band fallback)".to_string(),
            };
            eprintln!(
                "  asset: name={:?} kind={:?} ext={:?} id={} in_band={}B -> {action}",
                req.name,
                req.asset_type,
                req.file_extension,
                req.asset_id,
                req.in_band.map_or(0, <[u8]>::len),
            );
            supplied
        })
        .context("importing the .riv file (with asset loader)")?
    } else {
        ctx.load_file(&riv_bytes).context("importing the .riv file")?
    };

    // RIVE_LIST: print the selectable artboard names (and, below, the chosen
    // artboard's state-machine names), to discover what RIVE_ARTBOARD /
    // RIVE_STATE_MACHINE can pick.
    let list = std::env::var("RIVE_LIST").is_ok();
    if list {
        let names = file.artboard_names();
        println!("  artboards ({}): {:?}", names.len(), names);
    }

    // Artboard selection: RIVE_ARTBOARD="name" or RIVE_ARTBOARD_INDEX=N (else default).
    let artboard = if let Ok(name) = std::env::var("RIVE_ARTBOARD") {
        let name = name.trim();
        file.artboard_named(name)
            .with_context(|| format!("selecting artboard {name:?}"))?
    } else if let Ok(idx) = std::env::var("RIVE_ARTBOARD_INDEX") {
        let i: usize = idx.trim().parse().context("RIVE_ARTBOARD_INDEX must be an integer")?;
        file.artboard_at(i)
            .with_context(|| format!("selecting artboard at index {i}"))?
    } else {
        file.default_artboard()
            .context("instantiating the default artboard")?
    };

    if list {
        let names = artboard.state_machine_names();
        println!("  state machines ({}): {:?}", names.len(), names);
    }

    // State-machine selection: RIVE_STATE_MACHINE="name" or RIVE_SM_INDEX=N (else default).
    let mut state_machine = if let Ok(name) = std::env::var("RIVE_STATE_MACHINE") {
        let name = name.trim();
        artboard
            .state_machine_named(name)
            .with_context(|| format!("selecting state machine {name:?}"))?
    } else if let Ok(idx) = std::env::var("RIVE_SM_INDEX") {
        let i: usize = idx.trim().parse().context("RIVE_SM_INDEX must be an integer")?;
        artboard
            .state_machine_at(i)
            .with_context(|| format!("selecting state machine at index {i}"))?
    } else {
        artboard
            .default_state_machine()
            .context("instantiating the default state machine")?
    };

    // RIVE_FIT="fit[:alignment]" overrides the draw fit + alignment (default
    // contain:center). Proves selectable Fit — e.g. RIVE_FIT="none:bottomcenter"
    // renders the artboard at scale 1.0, anchored bottom-center (vs the default
    // letterboxed contain). Applied to BOTH the artboard (draw) and the state
    // machine (so pointer inversion stays aligned).
    if let Ok(spec) = std::env::var("RIVE_FIT") {
        let fa = parse_fit_align(&spec)?;
        artboard.set_fit_align(fa);
        state_machine.set_fit_align(fa);
        println!("  fit/align: {fa:?}");
    }

    // RIVE_TEXT_LIST: print the top-level text-run names + current values.
    // RIVE_TEXT_SET="name=value": set that run, then read it back; the change
    // shows in the rendered PNG. RIVE_TEXT_GET="name": just read a run. Proves
    // runtime text get/set + introspection.
    if std::env::var("RIVE_TEXT_LIST").is_ok() {
        let names = artboard.text_run_names();
        println!("  text runs ({}):", names.len());
        for name in &names {
            let val = artboard.text_get(name).unwrap_or_default();
            println!("    {name:?} = {val:?}");
        }
    }
    if let Ok(name) = std::env::var("RIVE_TEXT_GET") {
        println!("  text get {name:?} = {:?}", artboard.text_get(&name)?);
    }
    if let Ok(spec) = std::env::var("RIVE_TEXT_SET") {
        let (name, value) = spec
            .split_once('=')
            .context("RIVE_TEXT_SET must be name=value")?;
        artboard.text_set(name, value)?;
        println!(
            "  text set {name:?} -> {:?} (read-back: {:?})",
            value,
            artboard.text_get(name)?
        );
    }

    // --- Rig control (bones / constraints / solos) --------------------------
    // RIVE_RIG_LIST: print the authored bone / constraint / solo names (the
    // settable handles). RIVE_BONE_SET="name:prop=value" / RIVE_BONE_GET="name:prop"
    // (prop = rotation|scalex|scaley|length|x|y). RIVE_CONSTRAINT_SET="name=value" /
    // RIVE_CONSTRAINT_GET="name". RIVE_SOLO_SET="name=child" (or "name#index") /
    // RIVE_SOLO_GET="name". A set takes effect on the next advance — the change
    // shows in the rendered PNG.
    if std::env::var("RIVE_RIG_LIST").is_ok() {
        println!("  bones: {:?}", artboard.bone_names());
        println!("  constraints: {:?}", artboard.constraint_names());
        println!("  solos: {:?}", artboard.solo_names());
    }
    if let Ok(spec) = std::env::var("RIVE_BONE_GET") {
        let (name, prop) = spec.split_once(':').context("RIVE_BONE_GET must be name:prop")?;
        println!(
            "  bone get {name:?}.{prop} = {:?}",
            artboard.bone_get(name, parse_bone_prop(prop)?)?
        );
    }
    if let Ok(spec) = std::env::var("RIVE_BONE_SET") {
        let (name, rest) = spec.split_once(':').context("RIVE_BONE_SET must be name:prop=value")?;
        let (prop, value) = rest.split_once('=').context("RIVE_BONE_SET must be name:prop=value")?;
        let prop = parse_bone_prop(prop)?;
        let value: f32 = value.trim().parse().context("RIVE_BONE_SET value must be a float")?;
        artboard.bone_set(name, prop, value)?;
        println!(
            "  bone set {name:?}.{prop:?} -> {value} (read-back: {:?})",
            artboard.bone_get(name, prop)?
        );
    }
    if let Ok(name) = std::env::var("RIVE_CONSTRAINT_GET") {
        println!(
            "  constraint get {name:?}.strength = {:?}",
            artboard.constraint_get_strength(&name)?
        );
    }
    if let Ok(spec) = std::env::var("RIVE_CONSTRAINT_SET") {
        let (name, value) = spec.split_once('=').context("RIVE_CONSTRAINT_SET must be name=value")?;
        let value: f32 = value.trim().parse().context("RIVE_CONSTRAINT_SET value must be a float")?;
        artboard.constraint_set_strength(name, value)?;
        println!(
            "  constraint set {name:?}.strength -> {value} (read-back: {:?})",
            artboard.constraint_get_strength(name)?
        );
    }
    if let Ok(name) = std::env::var("RIVE_SOLO_GET") {
        println!(
            "  solo get {name:?} active = {:?} (index {:?})",
            artboard.solo_get_active(&name)?,
            artboard.solo_get_active_index(&name)
        );
    }
    if let Ok(spec) = std::env::var("RIVE_SOLO_SET") {
        // "name=child" selects by name; "name=#index" selects by 0-based index.
        let (name, child) = spec
            .split_once('=')
            .context("RIVE_SOLO_SET must be name=child or name=#index")?;
        if let Some(idx) = child.strip_prefix('#') {
            let idx: usize = idx.trim().parse().context("RIVE_SOLO_SET index must be an integer")?;
            artboard.solo_set_active_index(name, idx)?;
        } else {
            artboard.solo_set_active(name, child)?;
        }
        println!(
            "  solo set {name:?} -> {child:?} (read-back: {:?})",
            artboard.solo_get_active(name)?
        );
    }

    // --- Runtime input (joystick / keyboard / gamepad / focus) --------------
    // RIVE_JOYSTICK_LIST prints authored joystick names; RIVE_JOYSTICK_SET="name=x,y"
    // sets one (drives linked anims — shows in the rendered PNG); RIVE_JOYSTICK_GET="name"
    // reads it. RIVE_KEY="key[:up][:repeat]" feeds a key (key = a letter/digit or a name
    // like space/enter/left); RIVE_TEXT="..." feeds text. RIVE_GAMEPAD_BTN="idx[=value]"
    // (W3C button 0..=16) / RIVE_GAMEPAD_AXIS="idx=value" (axis 0..=5) feed a pad.
    // RIVE_FOCUS="next|prev|left|right|up|down|clear" drives focus; RIVE_FOCUS_STATE prints
    // it. Keyboard/gamepad/focus only DO something when the .riv authors focus + listeners
    // (they print whether a listener consumed — false on a plain asset).
    if std::env::var("RIVE_JOYSTICK_LIST").is_ok() {
        println!("  joysticks: {:?}", artboard.joystick_names());
    }
    if let Ok(name) = std::env::var("RIVE_JOYSTICK_GET") {
        let (x, y) = artboard.joystick_get(&name)?;
        println!("  joystick get {name:?} = ({x}, {y})");
    }
    if let Ok(spec) = std::env::var("RIVE_JOYSTICK_SET") {
        let (name, xy) = spec.split_once('=').context("RIVE_JOYSTICK_SET must be name=x,y")?;
        let (x, y) = parse_xy(xy).context("RIVE_JOYSTICK_SET must be name=x,y")?;
        artboard.joystick_set(name, x, y)?;
        println!(
            "  joystick set {name:?} -> ({x}, {y}) (read-back: {:?})",
            artboard.joystick_get(name)?
        );
    }
    if let Ok(spec) = std::env::var("RIVE_KEY") {
        // "key", "key:up", "key:down:repeat" — pressed unless ":up".
        let mut parts = spec.split(':');
        let key = parse_key(parts.next().unwrap_or(""))?;
        let (mut pressed, mut repeat) = (true, false);
        for p in parts {
            match p.trim().to_ascii_lowercase().as_str() {
                "up" => pressed = false,
                "down" => pressed = true,
                "repeat" => repeat = true,
                other => anyhow::bail!("unknown RIVE_KEY flag {other:?} (up|down|repeat)"),
            }
        }
        let consumed =
            state_machine.key_input(key, rive_renderer::KeyModifiers::NONE, pressed, repeat);
        println!("  key {key:?} pressed={pressed} repeat={repeat} -> consumed={consumed}");
    }
    if let Ok(text) = std::env::var("RIVE_TEXT") {
        println!("  text {text:?} -> consumed={}", state_machine.text_input(&text));
    }
    if let Ok(spec) = std::env::var("RIVE_GAMEPAD_BTN") {
        let (idx, value) = match spec.split_once('=') {
            Some((i, v)) => (i, v.trim().parse().context("gamepad button value must be a float")?),
            None => (spec.as_str(), 1.0_f32),
        };
        let button = parse_gamepad_button(idx)?;
        let consumed = state_machine.gamepad_button(button, value);
        println!("  gamepad button {button:?} value={value} -> consumed={consumed}");
    }
    if let Ok(spec) = std::env::var("RIVE_GAMEPAD_AXIS") {
        let (idx, value) = spec.split_once('=').context("RIVE_GAMEPAD_AXIS must be idx=value")?;
        let value: f32 = value.trim().parse().context("gamepad axis value must be a float")?;
        let axis = parse_gamepad_axis(idx)?;
        println!(
            "  gamepad axis {axis:?} value={value} -> consumed={}",
            state_machine.gamepad_axis(axis, value)
        );
    }
    if let Ok(spec) = std::env::var("RIVE_FOCUS") {
        if spec.trim().eq_ignore_ascii_case("clear") {
            state_machine.clear_focus();
            println!("  focus cleared");
        } else {
            let dir = parse_focus_dir(&spec)?;
            println!("  focus {dir:?} -> moved={}", state_machine.focus_advance(dir));
        }
    }
    if std::env::var("RIVE_FOCUS_STATE").is_ok() {
        let st = state_machine.focus_state();
        println!(
            "  focus state: has_focus={} expects_keyboard={}",
            st.has_focus, st.expects_keyboard
        );
    }

    // RIVE_VM_DUMP: print the artboard's view-model property schema (name + kind),
    // recursing into nested view models and list items via the handle API — use it
    // to discover real property names for RIVE_VM_SET / RIVE_VM_GET.
    if std::env::var("RIVE_VM_DUMP").is_ok() {
        use rive_renderer::{Artboard, RiveValueKind, RiveViewModelInstance};
        // Recurse a VM instance. `path` is the `/`-route from the root and is only
        // valid while `addressable` (true for the root + named-nested VMs); once we
        // descend into a list item it goes false (list items can't be `/`-addressed,
        // so enum-label lookup — which is path-based — is skipped there).
        fn dump(
            ab: &Artboard,
            vmi: &RiveViewModelInstance,
            path: &str,
            addressable: bool,
            indent: usize,
            depth: usize,
        ) {
            let pad = "  ".repeat(indent);
            for (name, kind) in vmi.properties() {
                let child_path = if path.is_empty() { name.clone() } else { format!("{path}/{name}") };
                print!("{pad}{name:?}: {kind:?}");
                match kind {
                    RiveValueKind::Enum => {
                        if addressable {
                            if let Ok(vals) = ab.vm_enum_values(&child_path) {
                                print!(" {vals:?}");
                            }
                        }
                        println!();
                    }
                    RiveValueKind::ViewModel if depth > 0 => {
                        println!();
                        if let Some(child) = vmi.view_model(&name) {
                            dump(ab, &child, &child_path, addressable, indent + 1, depth - 1);
                        }
                    }
                    RiveValueKind::List if depth > 0 => {
                        let n = vmi.list_size(&name).unwrap_or(0);
                        println!(" (len {n})");
                        for i in 0..n {
                            if let Some(item) = vmi.list_item(&name, i) {
                                println!("{pad}  [{i}]:");
                                dump(ab, &item, "", false, indent + 2, depth - 1);
                            }
                        }
                    }
                    _ => println!(),
                }
            }
        }
        match artboard.vm_root() {
            Some(root) => {
                println!("  view-model: {} top-level propertie(s)", root.properties().len());
                dump(&artboard, &root, "", true, 2, 4);
            }
            None => println!("  view-model: none"),
        }
    }

    // RIVE_VM_SET="path=value" writes a view-model property BEFORE advancing (so
    // the state machine / script observes it this tick). `=true`/`=false` set a
    // bool; anything else parses as a number. A `name[i]` segment indexes a list
    // element (e.g. "wheels[2]/value") via `vm_resolve` — what a flat path can't do.
    // RIVE_VM_FORCE_RESOLVE=1 routes *every* path through `vm_resolve` (diagnostic:
    // exercise the resolver's nested-VM walk on a plain `/`-path that has no list).
    let force_resolve = std::env::var("RIVE_VM_FORCE_RESOLVE").is_ok();
    if let Ok(spec) = std::env::var("RIVE_VM_SET") {
        if let Some((path, val)) = spec.split_once('=') {
            let (path, val) = (path.trim(), val.trim());
            enum SetVal {
                Bool(bool),
                Number(f32),
            }
            let sv = match val {
                "true" => SetVal::Bool(true),
                "false" => SetVal::Bool(false),
                _ => SetVal::Number(
                    val.parse()
                        .context("RIVE_VM_SET value must be a number or true/false")?,
                ),
            };
            let res = if force_resolve || path.contains('[') {
                let (item, leaf) = artboard
                    .vm_resolve(path)
                    .with_context(|| format!("resolving view-model path {path:?}"))?;
                match sv {
                    SetVal::Bool(b) => item.set_bool(&leaf, b),
                    SetVal::Number(n) => item.set_number(&leaf, n),
                }
            } else {
                match sv {
                    SetVal::Bool(b) => artboard.vm_set_bool(path, b),
                    SetVal::Number(n) => artboard.vm_set_number(path, n),
                }
            };
            res.with_context(|| format!("setting view-model property {path:?}"))?;
            println!("  set view-model {path:?} = {val}");
        }
    }

    // RIVE_VM_SET_ENUM="path=index" sets an enum property by index before advancing
    // (e.g. drive `viseme` to change the mouth shape — a visible write to diff).
    if let Ok(spec) = std::env::var("RIVE_VM_SET_ENUM") {
        if let Some((path, idx)) = spec.split_once('=') {
            let (path, idx) = (path.trim(), idx.trim());
            let index: u32 = idx
                .parse()
                .context("RIVE_VM_SET_ENUM index must be an integer")?;
            let res = if force_resolve || path.contains('[') {
                let (item, leaf) = artboard
                    .vm_resolve(path)
                    .with_context(|| format!("resolving view-model path {path:?}"))?;
                item.set_enum_index(&leaf, index)
            } else {
                artboard.vm_set_enum_index(path, index)
            };
            res.with_context(|| format!("setting enum {path:?} = index {index}"))?;
            println!("  set view-model enum {path:?} = index {index}");
        }
    }

    // RIVE_VM_SET_IMAGE="path=imagefile" decodes an encoded image (PNG/JPEG/WEBP)
    // and binds it to a view-model image property before advancing — a visible write
    // to diff (e.g. bind open_source.jpg to image_fit_alignment.riv's "imageProperty").
    // An EMPTY file ("path=") instead *clears* the property (unbind). A `name[i]`
    // segment indexes a list element via `vm_resolve` (handle set_image/clear_image).
    if let Ok(spec) = std::env::var("RIVE_VM_SET_IMAGE") {
        if let Some((path, file)) = spec.split_once('=') {
            let (path, file) = (path.trim(), file.trim());
            // Decode the image up front (None => clear). Keeps it alive across the bind.
            let image = if file.is_empty() {
                None
            } else {
                let bytes = std::fs::read(file).with_context(|| format!("reading image file {file:?}"))?;
                Some(
                    ctx.decode_image(&bytes)
                        .with_context(|| format!("decoding image {file:?}"))?,
                )
            };
            let res = if force_resolve || path.contains('[') {
                let (item, leaf) = artboard
                    .vm_resolve(path)
                    .with_context(|| format!("resolving view-model path {path:?}"))?;
                match &image {
                    Some(img) => item.set_image(&leaf, img),
                    None => item.clear_image(&leaf),
                }
            } else {
                match &image {
                    Some(img) => artboard.vm_set_image(path, img),
                    None => artboard.vm_clear_image(path),
                }
            };
            res.with_context(|| format!("binding image to {path:?}"))?;
            match &image {
                Some(_) => println!("  set view-model image {path:?} = {file}"),
                None => println!("  cleared view-model image {path:?}"),
            }
        }
    }

    // RIVE_NESTED_LIST: list nested-artboard component names + each child's drivable
    // handles (the child is itself an Artboard, so the same rig/text/joystick getters
    // introspect it). Discovery for RIVE_NESTED below.
    if std::env::var("RIVE_NESTED_LIST").is_ok() {
        let names = artboard.nested_artboard_names();
        println!("  nested artboards ({}): {:?}", names.len(), names);
        for n in &names {
            if let Ok(child) = artboard.nested_artboard(n) {
                println!(
                    "    {n:?}: nested={:?} bones={:?} solos={:?} texts={:?} joysticks={:?}",
                    child.nested_artboard_names(),
                    child.bone_names(),
                    child.solo_names(),
                    child.text_run_names(),
                    child.joystick_names(),
                );
            }
        }
    }
    // RIVE_NESTED="child/path" (or "#index" for unnamed components) resolves a nested
    // child; the SAME rig/text setters then drive it (rendered as part of the parent):
    // RIVE_NESTED_BONE_SET="name:prop=value", RIVE_NESTED_SOLO_SET="name=child",
    // RIVE_NESTED_TEXT_SET="name=value". Bone/solo names may be "" in some assets.
    if let Ok(p) = std::env::var("RIVE_NESTED") {
        let p = p.trim();
        let child = if let Some(idx) = p.strip_prefix('#') {
            let idx: usize = idx.trim().parse().context("RIVE_NESTED \"#index\" must be an integer")?;
            artboard
                .nested_artboard_at(idx)
                .with_context(|| format!("resolving nested artboard #{idx}"))?
        } else {
            artboard
                .nested_artboard_at_path(p)
                .with_context(|| format!("resolving nested artboard {p:?}"))?
        };
        println!(
            "  nested {p:?}: bones={:?} solos={:?} texts={:?}",
            child.bone_names(),
            child.solo_names(),
            child.text_run_names()
        );
        if let Ok(spec) = std::env::var("RIVE_NESTED_BONE_SET") {
            let (name, rest) = spec.split_once(':').context("RIVE_NESTED_BONE_SET must be name:prop=value")?;
            let (prop, value) = rest.split_once('=').context("RIVE_NESTED_BONE_SET must be name:prop=value")?;
            let prop = parse_bone_prop(prop)?;
            let value: f32 = value.trim().parse().context("RIVE_NESTED_BONE_SET value must be a float")?;
            child.bone_set(name, prop, value)?;
            println!("  nested bone set {name:?} {prop:?}={value} (read-back {:?})", child.bone_get(name, prop)?);
        }
        if let Ok(spec) = std::env::var("RIVE_NESTED_SOLO_SET") {
            let (name, child_name) = spec.split_once('=').context("RIVE_NESTED_SOLO_SET must be name=child")?;
            child.solo_set_active(name, child_name)?;
            println!("  nested solo set {name:?} = {child_name:?}");
        }
        if let Ok(spec) = std::env::var("RIVE_NESTED_TEXT_SET") {
            let (name, value) = spec.split_once('=').context("RIVE_NESTED_TEXT_SET must be name=value")?;
            child.text_set(name, value)?;
            println!("  nested text set {name:?} = {value:?}");
        }
    }
    // RIVE_VM_SET_ARTBOARD="path=artboardName" binds a file artboard to a view-model
    // artboard-reference (propertyArtboard) property; "path=" clears it.
    if let Ok(spec) = std::env::var("RIVE_VM_SET_ARTBOARD") {
        if let Some((path, ab_name)) = spec.split_once('=') {
            let (path, ab_name) = (path.trim(), ab_name.trim());
            if ab_name.is_empty() {
                artboard.vm_clear_artboard(path)?;
                println!("  cleared view-model artboard {path:?}");
            } else {
                let ba = file
                    .bindable_artboard_named(ab_name)
                    .with_context(|| format!("creating bindable artboard {ab_name:?}"))?;
                artboard.vm_set_artboard(path, &ba)?;
                println!("  set view-model artboard {path:?} = {ab_name}");
            }
        }
    }

    // Advance the state machine, then render a single offscreen snapshot.
    // RIVE_ADVANCE_FRAMES (default 1) ticks autonomous scripts / animations
    // forward N 60Hz frames before the snapshot, so two runs at different frame
    // counts can be diffed to prove a scripted animation (e.g. BallBreath) runs.
    let mut advance_frames: u32 = std::env::var("RIVE_ADVANCE_FRAMES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
        .max(1);
    // RIVE_REALTIME_SECS="N": advance in WALL-CLOCK real time (~60Hz with real
    // sleeps) for N seconds instead of N instant frames, keeping the process alive
    // so rive's audio device thread actually plays. Needed to HEAR audio — audio
    // events route to the OS output via miniaudio (--with_rive_audio=system), which
    // mixes on a background thread; an instant advance + exit would cut it off.
    // Overrides RIVE_ADVANCE_FRAMES.
    let realtime_secs: Option<f32> = std::env::var("RIVE_REALTIME_SECS")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .filter(|s: &f32| *s > 0.0);
    if let Some(secs) = realtime_secs {
        advance_frames = (secs * 60.0).ceil() as u32;
        println!("  realtime: {secs}s ({advance_frames} frames @ 60Hz)");
    }
    // Audio bridge knobs (--with_rive_audio=system: rive plays audio events to the
    // OS output during advance). RIVE_AUDIO_START=1 pre-opens the device (proves it
    // opens even before any audio event). RIVE_AUDIO_VOLUME="v" sets the master
    // volume (0 = mute, 1 = unity). Pair with RIVE_REALTIME_SECS to actually hear it.
    if std::env::var("RIVE_AUDIO_START").is_ok() {
        let ok = rive_renderer::audio::start();
        println!(
            "  audio: available={} started={ok}",
            rive_renderer::audio::is_available()
        );
    }
    if let Some(v) = std::env::var("RIVE_AUDIO_VOLUME")
        .ok()
        .and_then(|s| s.trim().parse::<f32>().ok())
    {
        rive_renderer::audio::set_volume(v);
        println!("  audio volume: {v}");
    }
    // RIVE_POINTER="x,y" (target-pixel space, top-left origin) forwards a pointer
    // move each frame before advancing, so pointer-driven Listeners / joysticks
    // (e.g. an eye that follows the cursor) respond. Two runs at different
    // positions can be diffed to prove pointer input reaches the state machine.
    let pointer = std::env::var("RIVE_POINTER").ok().and_then(|s| parse_xy(&s));
    // RIVE_POINTER_TILE="tw,th" simulates the zero-copy ATLAS path: an atlas face is
    // drawn into a tile of this size (via draw_viewport), so pointer coords (in the
    // face's target space) are normalized into the tile before the Fit/Alignment is
    // inverted. For a SQUARE target the mapping is scale-invariant — ANY tile size
    // yields the same artboard hit as the dedicated full-target path (the tile is an
    // internal LOD detail, invisible to interaction). Proves `set_pointer_tile`. Unset
    // / (0,0) = full-target inversion (the dedicated default).
    if let Some((tw, th)) = std::env::var("RIVE_POINTER_TILE")
        .ok()
        .and_then(|s| parse_xy(&s))
    {
        state_machine.set_pointer_tile(tw, th);
        println!("  pointer tile: {tw}x{th}");
    }
    // RIVE_FLICK="x0,y0,x1,y1[,drag_frames]" simulates a press-drag-release gesture
    // (target-pixel space) to drive interaction-gated content — e.g. flicking the
    // big-wheel to spin it, which fires its timeline audio. Press at (x0,y0) on frame
    // 0, drag linearly to (x1,y1) over drag_frames (default 8), release, then keep
    // advancing. Pair with RIVE_DUMP_PCM to capture the resulting audio (external mode).
    let flick: Option<(f32, f32, f32, f32, u32)> = std::env::var("RIVE_FLICK").ok().and_then(|s| {
        let p: Vec<f32> = s.split(',').filter_map(|x| x.trim().parse().ok()).collect();
        match p.len() {
            4 => Some((p[0], p[1], p[2], p[3], 8)),
            5 => Some((p[0], p[1], p[2], p[3], p[4] as u32)),
            _ => None,
        }
    });
    if let Some(f) = flick {
        println!("  flick gesture: {f:?}");
    }
    // RIVE_VM_OBSERVE="path1,path2" observes change/trigger fires via the modern
    // data-binding read-back (the non-deprecated replacement for events read-back):
    // PRIME each path (subscribe before advancing), then after every advance report
    // which changed / fired. Proves `vm_flush_changed` end-to-end — e.g. observing
    // a script-driven number like "breath/scaleX" reports a change each frame.
    let observe: Vec<String> = std::env::var("RIVE_VM_OBSERVE")
        .ok()
        .map(|s| {
            s.split(',')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect()
        })
        .unwrap_or_default();
    for path in &observe {
        // Prime: subscribe before the first advance; discard the initial flag.
        let _ = artboard.vm_flush_changed(path);
    }
    let mut observe_fires: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    // RIVE_DUMP_PCM="out.wav" (audio-external feature only): in external mode rive owns
    // NO device — the host pulls the mixed PCM. This pulls one video-frame of audio
    // (sample_rate/60 frames) after each advance and writes a 16-bit WAV, proving rive
    // mixes REAL audio into host-pulled PCM. The reported peak/RMS being non-zero is the
    // CI-friendly assertion that audio is flowing without needing a device to open.
    #[cfg(feature = "audio-external")]
    let dump_path = std::env::var("RIVE_DUMP_PCM").ok();
    #[cfg(feature = "audio-external")]
    let (dump_channels, dump_sr) = (
        rive_renderer::audio::external::channels().max(1),
        rive_renderer::audio::external::sample_rate().max(1),
    );
    #[cfg(feature = "audio-external")]
    let dump_frames_per_advance = (dump_sr / 60).max(1) as usize;
    #[cfg(feature = "audio-external")]
    let mut dump_samples: Vec<f32> = Vec::new();
    #[cfg(feature = "audio-external")]
    if dump_path.is_some() {
        println!("  audio external: pulling {dump_channels}ch @ {dump_sr}Hz, {dump_frames_per_advance} frames/advance");
    }
    for i in 0..advance_frames {
        if let Some((x0, y0, x1, y1, drag)) = flick {
            let drag = drag.max(1);
            if i == 0 {
                state_machine.pointer_down(x0, y0, width, height);
            } else if i <= drag {
                let t = i as f32 / drag as f32;
                state_machine.pointer_move(x0 + (x1 - x0) * t, y0 + (y1 - y0) * t, width, height);
            } else if i == drag + 1 {
                state_machine.pointer_up(x1, y1, width, height);
            }
        } else if let Some((px, py)) = pointer {
            state_machine.pointer_move(px, py, width, height);
        }
        state_machine.advance(FRAME_DT_SECONDS);
        for path in &observe {
            if artboard.vm_flush_changed(path).unwrap_or(false) {
                *observe_fires.entry(path.clone()).or_insert(0) += 1;
            }
        }
        // External audio: pull this frame's mixed PCM right after advance (which fired
        // any audio events), keeping the engine clock in lockstep with the timeline.
        #[cfg(feature = "audio-external")]
        if dump_path.is_some() {
            let mut buf = vec![0.0f32; dump_frames_per_advance * dump_channels as usize];
            let n = rive_renderer::audio::external::read_frames(&mut buf);
            dump_samples.extend_from_slice(&buf[..n * dump_channels as usize]);
        }
        // Pace to wall-clock in realtime mode so audio plays audibly (see above).
        if realtime_secs.is_some() {
            std::thread::sleep(std::time::Duration::from_secs_f32(FRAME_DT_SECONDS));
        }
    }
    #[cfg(feature = "audio-external")]
    if let Some(path) = dump_path {
        write_wav_and_report(&path, &dump_samples, dump_channels as u16, dump_sr)?;
    }
    for path in &observe {
        let n = observe_fires.get(path).copied().unwrap_or(0);
        println!("  view-model observe {path:?}: changed/fired in {n}/{advance_frames} frame(s)");
    }

    // RIVE_VM_GET="path" reads a view-model number AFTER advancing — read-back of
    // a value the script / state machine wrote this frame. A `name[i]` segment
    // indexes a list element (e.g. "wheels[2]/value"), resolved via `vm_resolve`.
    if let Ok(path) = std::env::var("RIVE_VM_GET") {
        let path = path.trim();
        let read = if force_resolve || path.contains('[') {
            artboard
                .vm_resolve(path)
                .and_then(|(item, leaf)| item.get_number(&leaf))
        } else {
            artboard.vm_get_number(path)
        };
        match read {
            Ok(v) => println!("  view-model {path:?} = {v}"),
            Err(e) => println!("  view-model {path:?} read failed: {e}"),
        }
    }

    // RIVE_SEEK="t": seek the scene to absolute time `t` (seconds) AFTER advancing,
    // right before the snapshot — so the rendered pose is exactly time `t` (scrubbing).
    // Only LINEAR-ANIMATION scenes are seekable (an artboard with animations but no
    // state machine, e.g. a raw animation .riv); on a state machine `seek` returns
    // false and the frame is unchanged. The duration/time read-back below proves
    // seekability and the new playhead. Two runs at different `t` diff to a different
    // PNG (proves the seek moved the playhead); same `t` twice is byte-identical.
    println!(
        "  playback: duration={:?} time={:?} (None == state machine, not seekable)",
        state_machine.duration(),
        state_machine.time()
    );
    if let Ok(spec) = std::env::var("RIVE_SEEK") {
        let t: f32 = spec.trim().parse().context("RIVE_SEEK must be a float (seconds)")?;
        let ok = state_machine.seek(t);
        println!(
            "  seek to {t}s: applied={ok} (playhead now {:?})",
            state_machine.time()
        );
    }

    let frame = ctx
        .begin_frame(&target, CLEAR_RGBA)
        .context("beginning the frame")?;
    frame.draw(&artboard).context("drawing the artboard")?;
    frame.flush().context("flushing the frame")?;

    // Read back the premultiplied, top-down RGBA8 image and convert to straight
    // alpha for a viewer-correct PNG (a no-op here because the clear is opaque).
    let mut pixels = target
        .read_pixels_to_vec()
        .context("reading pixels back")?;
    unpremultiply_rgba8(&mut pixels);

    let non_clear = count_non_background(&pixels, CLEAR_RGBA);
    println!(
        "  read back {} bytes; {} / {} pixels differ from the clear color",
        pixels.len(),
        non_clear,
        width as usize * height as usize
    );
    anyhow::ensure!(
        non_clear > 0,
        "every pixel equals the clear color — the artboard did not render \
         (wrong state machine, zero-size artboard, or a GPU/path issue)"
    );

    write_png(&output, width, height, &pixels)
        .with_context(|| format!("writing PNG {}", output.display()))?;

    println!("wrote {}", output.display());
    Ok(())
}

/// Parses an `"x,y"` pair of `f32`s (for `RIVE_POINTER`). `None` if malformed.
fn parse_xy(s: &str) -> Option<(f32, f32)> {
    let (a, b) = s.split_once(',')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

/// Parses a bone property name (case-insensitive) into a [`rive_renderer::BoneProp`]
/// for the `RIVE_BONE_GET` / `RIVE_BONE_SET` knobs.
fn parse_bone_prop(s: &str) -> Result<rive_renderer::BoneProp> {
    use rive_renderer::BoneProp;
    Ok(match s.trim().to_ascii_lowercase().as_str() {
        "rotation" | "rot" => BoneProp::Rotation,
        "scalex" => BoneProp::ScaleX,
        "scaley" => BoneProp::ScaleY,
        "length" | "len" => BoneProp::Length,
        "x" => BoneProp::X,
        "y" => BoneProp::Y,
        other => anyhow::bail!("unknown bone prop {other:?} (rotation|scalex|scaley|length|x|y)"),
    })
}

/// Parses a key for the `RIVE_KEY` knob: a single letter (`a`..`z`) / digit
/// (`0`..`9`), or a named key (`space`, `enter`, arrows, …).
fn parse_key(s: &str) -> Result<rive_renderer::Key> {
    use rive_renderer::Key;
    #[rustfmt::skip]
    const LETTERS: [Key; 26] = [
        Key::A, Key::B, Key::C, Key::D, Key::E, Key::F, Key::G, Key::H, Key::I, Key::J, Key::K,
        Key::L, Key::M, Key::N, Key::O, Key::P, Key::Q, Key::R, Key::S, Key::T, Key::U, Key::V,
        Key::W, Key::X, Key::Y, Key::Z,
    ];
    #[rustfmt::skip]
    const DIGITS: [Key; 10] = [
        Key::Key0, Key::Key1, Key::Key2, Key::Key3, Key::Key4,
        Key::Key5, Key::Key6, Key::Key7, Key::Key8, Key::Key9,
    ];
    let t = s.trim();
    if let [c] = t.as_bytes() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() {
            return Ok(LETTERS[(c - b'a') as usize]);
        }
        if c.is_ascii_digit() {
            return Ok(DIGITS[(c - b'0') as usize]);
        }
    }
    Ok(match t.to_ascii_lowercase().as_str() {
        "space" => Key::Space,
        "enter" | "return" => Key::Enter,
        "tab" => Key::Tab,
        "escape" | "esc" => Key::Escape,
        "backspace" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "left" => Key::Left,
        "right" => Key::Right,
        "up" => Key::Up,
        "down" => Key::Down,
        other => anyhow::bail!("unknown key {other:?} (a-z, 0-9, space|enter|tab|escape|arrows|…)"),
    })
}

/// Parses a W3C button index (`0`..=`16`) for `RIVE_GAMEPAD_BTN`.
fn parse_gamepad_button(s: &str) -> Result<rive_renderer::GamepadButton> {
    use rive_renderer::GamepadButton::*;
    #[rustfmt::skip]
    const B: [rive_renderer::GamepadButton; 17] = [
        South, East, West, North, LeftShoulder, RightShoulder, LeftTrigger, RightTrigger, Back,
        Forward, LeftStick, RightStick, DpadUp, DpadDown, DpadLeft, DpadRight, Start,
    ];
    let i: usize = s.trim().parse().context("gamepad button must be a 0..=16 index")?;
    B.get(i).copied().context("gamepad button index out of range (0..=16)")
}

/// Parses a W3C axis index (`0`..=`5`) for `RIVE_GAMEPAD_AXIS`.
fn parse_gamepad_axis(s: &str) -> Result<rive_renderer::GamepadAxis> {
    use rive_renderer::GamepadAxis::*;
    const A: [rive_renderer::GamepadAxis; 6] = [LeftX, LeftY, RightX, RightY, LeftTrigger, RightTrigger];
    let i: usize = s.trim().parse().context("gamepad axis must be a 0..=5 index")?;
    A.get(i).copied().context("gamepad axis index out of range (0..=5)")
}

/// Parses a focus direction for `RIVE_FOCUS` (`clear` is handled by the caller).
fn parse_focus_dir(s: &str) -> Result<rive_renderer::FocusDir> {
    use rive_renderer::FocusDir;
    Ok(match s.trim().to_ascii_lowercase().as_str() {
        "next" => FocusDir::Next,
        "prev" | "previous" => FocusDir::Prev,
        "left" => FocusDir::Left,
        "right" => FocusDir::Right,
        "up" => FocusDir::Up,
        "down" => FocusDir::Down,
        other => anyhow::bail!("unknown focus dir {other:?} (next|prev|left|right|up|down|clear)"),
    })
}

/// Parses `"fit[:alignment]"` (case-insensitive) into a [`rive_renderer::FitAlign`],
/// e.g. `"none:bottomcenter"`, `"fill"`, `"cover:topleft"`. Alignment defaults to
/// `center`.
fn parse_fit_align(spec: &str) -> Result<rive_renderer::FitAlign> {
    use rive_renderer::{Alignment, Fit, FitAlign};
    let (fit_s, align_s) = spec
        .split_once(':')
        .map_or((spec.trim(), "center"), |(f, a)| (f.trim(), a.trim()));
    let fit = match fit_s.to_ascii_lowercase().as_str() {
        "fill" => Fit::Fill,
        "contain" => Fit::Contain,
        "cover" => Fit::Cover,
        "fitwidth" => Fit::FitWidth,
        "fitheight" => Fit::FitHeight,
        "none" => Fit::None,
        "scaledown" => Fit::ScaleDown,
        "layout" => Fit::Layout,
        other => anyhow::bail!("unknown fit {other:?} (RIVE_FIT)"),
    };
    let alignment = match align_s.to_ascii_lowercase().as_str() {
        "topleft" => Alignment::TopLeft,
        "topcenter" => Alignment::TopCenter,
        "topright" => Alignment::TopRight,
        "centerleft" => Alignment::CenterLeft,
        "center" => Alignment::Center,
        "centerright" => Alignment::CenterRight,
        "bottomleft" => Alignment::BottomLeft,
        "bottomcenter" => Alignment::BottomCenter,
        "bottomright" => Alignment::BottomRight,
        other => anyhow::bail!("unknown alignment {other:?} (RIVE_FIT)"),
    };
    Ok(FitAlign {
        fit,
        alignment,
        scale_factor: 1.0,
    })
}

fn parse_or(arg: Option<String>, default: u32, name: &str) -> Result<u32> {
    match arg {
        None => Ok(default),
        Some(s) => s
            .parse()
            .with_context(|| format!("parsing {name} argument {s:?}")),
    }
}

/// Counts pixels whose color differs noticeably from the clear color — a cheap
/// "did anything actually draw?" sanity check.
fn count_non_background(pixels: &[u8], clear: [f32; 4]) -> usize {
    let to_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    let bg = [to_u8(clear[0]), to_u8(clear[1]), to_u8(clear[2]), to_u8(clear[3])];
    pixels
        .chunks_exact(4)
        .filter(|px| {
            // Allow a small tolerance for dithering / rounding.
            px.iter()
                .zip(bg.iter())
                .any(|(a, b)| a.abs_diff(*b) > 4)
        })
        .count()
}

fn write_png(path: &std::path::Path, width: u32, height: u32, rgba: &[u8]) -> Result<()> {
    let file = std::fs::File::create(path)?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(rgba)?;
    Ok(())
}

/// Reports peak/RMS of host-pulled rive PCM and writes it as a 16-bit PCM WAV (the
/// `audio-external` proof — no audio crate needed). `samples` is interleaved f32.
#[cfg(feature = "audio-external")]
fn write_wav_and_report(path: &str, samples: &[f32], channels: u16, sample_rate: u32) -> Result<()> {
    let peak = samples.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
    let rms = if samples.is_empty() {
        0.0
    } else {
        (samples.iter().map(|&s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
    };
    let frames = samples.len() / channels.max(1) as usize;
    println!(
        "  audio external: pulled {frames} frames ({:.2}s), peak={peak:.4} rms={rms:.4} -> {path}",
        frames as f32 / sample_rate.max(1) as f32
    );
    if peak == 0.0 {
        eprintln!("  WARNING: pulled PCM is pure silence (no audio events fired?)");
    }

    // Minimal canonical 16-bit PCM WAV (RIFF) — header + clamped i16 samples.
    let bits = 16u16;
    let block_align = channels * bits / 8;
    let byte_rate = sample_rate * u32::from(block_align);
    let data_bytes = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&bits.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_bytes.to_le_bytes());
    for &s in samples {
        let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(path, out)?;
    Ok(())
}
