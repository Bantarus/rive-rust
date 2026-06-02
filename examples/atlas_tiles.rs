//! M-SCALE Phase 1 — render N distinct artboards into ONE atlas via the new
//! `Frame::draw_viewport` (one begin / N tile draws / one flush), offscreen. Proves
//! the per-tile shim primitive places + scales each artboard into its own sub-rect
//! and confines it there (gutters stay transparent). No Bevy / wgpu — rive's own
//! offscreen device.
//!
//! Usage:
//!   cargo run --example atlas_tiles -- [OUT.png] [A.riv B.riv ...]
//!
//! Defaults: out_atlas.png, a 2x2 grid of {octopus, coffee, big-wheel, eye-joysticks}.

use std::io::BufWriter;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use rive_renderer::{unpremultiply_rgba8, Context};

const TILE: u32 = 512; // per-tile pixels (matches a near-LOD face)
const GUTTER: u32 = 4; // transparent border between tiles (the C2 writer-side gutter)
const COLS: u32 = 2;
const ROWS: u32 = 2;
// Transparent clear so "did this tile draw?" and "is the gutter clean?" are both
// readable off the alpha channel.
const CLEAR_RGBA: [f32; 4] = [0.0, 0.0, 0.0, 0.0];
const FRAME_DT_SECONDS: f32 = 1.0 / 60.0;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let output = args
        .next()
        .map_or_else(|| PathBuf::from("out_atlas.png"), PathBuf::from);
    let inputs: Vec<PathBuf> = {
        let rest: Vec<PathBuf> = args.map(PathBuf::from).collect();
        if rest.is_empty() {
            ["octopus_loop.riv", "coffee_loader.riv", "9939-18941-big-wheel-demo.riv", "5122-10308-eye-joysticks-demo.riv"]
                .iter()
                .map(|n| PathBuf::from("assets").join(n))
                .collect()
        } else {
            rest
        }
    };

    let atlas_w = COLS * TILE + (COLS + 1) * GUTTER;
    let atlas_h = ROWS * TILE + (ROWS + 1) * GUTTER;
    println!(
        "atlas {atlas_w}x{atlas_h} = {COLS}x{ROWS} tiles of {TILE}px (gutter {GUTTER}px), {} fixtures -> {}",
        inputs.len(),
        output.display()
    );

    let ctx = Context::new().context("creating the Rive Vulkan context")?;
    let target = ctx
        .offscreen_target(atlas_w, atlas_h)
        .context("creating the atlas offscreen target")?;

    // Load each fixture once; keep (artboard, state_machine) so the borrow lives
    // through the frame. Advance one frame so there is content to draw.
    let mut loaded = Vec::new();
    for path in &inputs {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let file = ctx.load_file(&bytes).with_context(|| format!("importing {}", path.display()))?;
        let artboard = file.default_artboard().context("default artboard")?;
        let mut sm = artboard.default_state_machine().context("default state machine")?;
        sm.advance(FRAME_DT_SECONDS);
        loaded.push((artboard, sm));
    }

    // Tile rects (x,y,w,h), row-major, each inset by the gutter.
    let n_tiles = (COLS * ROWS) as usize;
    let tile_rect = |i: u32| -> (f32, f32, f32, f32) {
        let (col, row) = (i % COLS, i / COLS);
        let x = (GUTTER + col * (TILE + GUTTER)) as f32;
        let y = (GUTTER + row * (TILE + GUTTER)) as f32;
        (x, y, TILE as f32, TILE as f32)
    };

    // ONE begin / N tile draws / ONE flush — the shippable batching shape.
    let frame = ctx.begin_frame(&target, CLEAR_RGBA).context("begin atlas frame")?;
    for i in 0..n_tiles {
        let (artboard, _) = &loaded[i % loaded.len()];
        let (x, y, w, h) = tile_rect(i as u32);
        frame
            .draw_viewport(artboard, x, y, w, h)
            .with_context(|| format!("draw_viewport tile {i}"))?;
    }
    frame.flush().context("flush atlas frame")?;

    let mut pixels = target.read_pixels_to_vec().context("read pixels")?;
    unpremultiply_rgba8(&mut pixels);

    // --- Automated placement/confinement checks (alpha channel) ---
    let alpha_at = |px: u32, py: u32| -> u8 {
        let idx = ((py * atlas_w + px) * 4 + 3) as usize;
        pixels.get(idx).copied().unwrap_or(0)
    };
    // (1) each tile drew content somewhere in its interior. Scan the interior on an
    //     8px grid (inset from the rect edge), so a sparse/hollow artboard — e.g. a
    //     thin loader outline whose dead center is transparent — still counts.
    let inset = 24u32;
    let mut empty_tiles = Vec::new();
    for i in 0..n_tiles as u32 {
        let (x, y, w, h) = tile_rect(i);
        let (x0, y0) = (x as u32 + inset, y as u32 + inset);
        let (x1, y1) = ((x + w) as u32 - inset, (y + h) as u32 - inset);
        let mut drew = 0u32;
        let mut yy = y0;
        while yy < y1 {
            let mut xx = x0;
            while xx < x1 {
                if alpha_at(xx, yy) > 8 {
                    drew += 1;
                }
                xx += 8;
            }
            yy += 8;
        }
        if drew < 16 {
            empty_tiles.push(i);
        }
    }
    // (2) the gutter columns/rows between tiles must be fully transparent (nothing
    //     bled out of a tile). Sample the vertical gutter between col 0 and col 1
    //     and the horizontal gutter between row 0 and row 1, down the middle.
    let vgut_x = GUTTER + TILE + GUTTER / 2; // center of the inter-column gutter
    let hgut_y = GUTTER + TILE + GUTTER / 2; // center of the inter-row gutter
    let mut gutter_bleed = 0u32;
    for p in 0..atlas_h {
        if alpha_at(vgut_x, p) > 0 {
            gutter_bleed += 1;
        }
    }
    for p in 0..atlas_w {
        if alpha_at(p, hgut_y) > 0 {
            gutter_bleed += 1;
        }
    }

    write_png(&output, atlas_w, atlas_h, &pixels)
        .with_context(|| format!("writing {}", output.display()))?;
    println!("wrote {}", output.display());
    println!(
        "  tiles drawn: {}/{}  | gutter-bleed pixels: {}",
        n_tiles - empty_tiles.len(),
        n_tiles,
        gutter_bleed
    );

    anyhow::ensure!(empty_tiles.is_empty(), "tiles did not render: {empty_tiles:?}");
    anyhow::ensure!(
        gutter_bleed == 0,
        "{gutter_bleed} gutter pixels are non-transparent — a tile bled past its rect (clip/placement bug)"
    );
    println!("OK: every tile rendered into its own rect; gutters clean (no bleed).");
    Ok(())
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
