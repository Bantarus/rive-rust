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

    // Load content and grab the default artboard + state machine.
    let file = ctx.load_file(&riv_bytes).context("importing the .riv file")?;
    let artboard = file
        .default_artboard()
        .context("instantiating the default artboard")?;
    let mut state_machine = artboard
        .default_state_machine()
        .context("instantiating the default state machine")?;

    // RIVE_VM_DUMP: print the artboard's view-model property schema (name + kind)
    // — use it to discover real property names for RIVE_VM_SET / RIVE_VM_GET.
    if std::env::var("RIVE_VM_DUMP").is_ok() {
        let props = artboard.vm_properties();
        println!("  view-model: {} propertie(s)", props.len());
        for (name, kind) in &props {
            println!("    {name:?}: {kind:?}");
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
    for _ in 0..advance_frames {
        if let Some((px, py)) = pointer {
            state_machine.pointer_move(px, py, width, height);
        }
        state_machine.advance(FRAME_DT_SECONDS);
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
