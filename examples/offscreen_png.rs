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
    // bool; anything else parses as a number.
    if let Ok(spec) = std::env::var("RIVE_VM_SET") {
        if let Some((path, val)) = spec.split_once('=') {
            let (path, val) = (path.trim(), val.trim());
            match val {
                "true" => artboard.vm_set_bool(path, true),
                "false" => artboard.vm_set_bool(path, false),
                _ => {
                    let n: f32 = val
                        .parse()
                        .context("RIVE_VM_SET value must be a number or true/false")?;
                    artboard.vm_set_number(path, n)
                }
            }
            .with_context(|| format!("setting view-model property {path:?}"))?;
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
            artboard
                .vm_set_enum_index(path, index)
                .with_context(|| format!("setting enum {path:?} = index {index}"))?;
            println!("  set view-model enum {path:?} = index {index}");
        }
    }

    // Advance the state machine, then render a single offscreen snapshot.
    // RIVE_ADVANCE_FRAMES (default 1) ticks autonomous scripts / animations
    // forward N 60Hz frames before the snapshot, so two runs at different frame
    // counts can be diffed to prove a scripted animation (e.g. BallBreath) runs.
    let advance_frames: u32 = std::env::var("RIVE_ADVANCE_FRAMES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
        .max(1);
    // RIVE_POINTER="x,y" (target-pixel space, top-left origin) forwards a pointer
    // move each frame before advancing, so pointer-driven Listeners / joysticks
    // (e.g. an eye that follows the cursor) respond. Two runs at different
    // positions can be diffed to prove pointer input reaches the state machine.
    let pointer = std::env::var("RIVE_POINTER").ok().and_then(|s| parse_xy(&s));
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
    for _ in 0..advance_frames {
        if let Some((px, py)) = pointer {
            state_machine.pointer_move(px, py, width, height);
        }
        state_machine.advance(FRAME_DT_SECONDS);
        for path in &observe {
            if artboard.vm_flush_changed(path).unwrap_or(false) {
                *observe_fires.entry(path.clone()).or_insert(0) += 1;
            }
        }
    }
    for path in &observe {
        let n = observe_fires.get(path).copied().unwrap_or(0);
        println!("  view-model observe {path:?}: changed/fired in {n}/{advance_frames} frame(s)");
    }

    // RIVE_VM_GET="path" reads a view-model number AFTER advancing — read-back of
    // a value the script / state machine wrote this frame.
    if let Ok(path) = std::env::var("RIVE_VM_GET") {
        let path = path.trim();
        match artboard.vm_get_number(path) {
            Ok(v) => println!("  view-model {path:?} = {v}"),
            Err(e) => println!("  view-model {path:?} read failed: {e}"),
        }
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
