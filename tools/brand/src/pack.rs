// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Container writers for PNG, ICO and ICNS files.
//!
//! Pixmaps hold premultiplied pixels, but the formats store straight RGBA,
//! so every writer converts first. The output has no timestamps, so the same
//! input always gives the same bytes.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use resvg::tiny_skia::Pixmap;

/// Returns the pixels of `p` as straight (non-premultiplied) RGBA bytes.
pub fn straight_rgba(p: &Pixmap) -> Vec<u8> {
    p.pixels()
        .iter()
        .flat_map(|c| {
            let c = c.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect()
}

/// Writes `bytes` to `path`, replacing any existing file.
fn save(path: &Path, bytes: &[u8]) -> Result<()> {
    fs::write(path, bytes).with_context(|| format!("failed to write {}", path.display()))
}

/// Writes `p` as a PNG file.
pub fn write_png(p: &Pixmap, path: &Path) -> Result<()> {
    let mut buf = Vec::new();
    let mut encoder = png::Encoder::new(&mut buf, p.width(), p.height());
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .context("failed to write PNG header")?;
    writer
        .write_image_data(&straight_rgba(p))
        .context("failed to write PNG data")?;
    writer.finish().context("failed to finish PNG")?;
    save(path, &buf)
}

/// Writes `images` as one ICO file, one entry per pixmap.
///
/// Each pixmap must be at most 256 pixels on each side.
pub fn write_ico(images: &[Pixmap], path: &Path) -> Result<()> {
    let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
    for p in images {
        let (w, h) = (p.width(), p.height());
        if !(1..=256).contains(&w) || !(1..=256).contains(&h) {
            bail!("ICO entries must be 1 to 256 px per side, got {w}x{h}");
        }
        let image = ico::IconImage::from_rgba_data(w, h, straight_rgba(p));
        let entry = ico::IconDirEntry::encode(&image).context("failed to encode ICO entry")?;
        dir.add_entry(entry);
    }
    let mut buf = Vec::new();
    dir.write(&mut buf).context("failed to write ICO")?;
    save(path, &buf)
}

/// Writes `images` as one ICNS file, one 32-bit RGBA element per pixmap.
///
/// Each pixmap must be square and one of 16, 32, 64, 128, 256, 512 or 1024
/// pixels; 1024 px is stored as the 512 pt @2x element.
pub fn write_icns(images: &[Pixmap], path: &Path) -> Result<()> {
    let mut family = icns::IconFamily::new();
    for p in images {
        let icon_type = icns_type(p.width(), p.height())?;
        let image = icns::Image::from_data(
            icns::PixelFormat::RGBA,
            p.width(),
            p.height(),
            straight_rgba(p),
        )
        .context("failed to build ICNS image")?;
        family
            .add_icon_with_type(&image, icon_type)
            .context("failed to encode ICNS element")?;
    }
    let mut buf = Vec::new();
    family.write(&mut buf).context("failed to write ICNS")?;
    save(path, &buf)
}

/// Returns the RGBA32 element type for a square icon of `w` x `h` pixels.
fn icns_type(w: u32, h: u32) -> Result<icns::IconType> {
    use icns::IconType::*;
    Ok(match (w, h) {
        (16, 16) => RGBA32_16x16,
        (32, 32) => RGBA32_32x32,
        (64, 64) => RGBA32_64x64,
        (128, 128) => RGBA32_128x128,
        (256, 256) => RGBA32_256x256,
        (512, 512) => RGBA32_512x512,
        (1024, 1024) => RGBA32_512x512_2x,
        _ => bail!("no ICNS RGBA type for {w}x{h}"),
    })
}
