// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Writes the desktop and tray assets.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use resvg::tiny_skia::Pixmap;

use crate::pack::{write_icns, write_ico, write_png};
use crate::render::{render_svg, tint, to_template};
use crate::sources::{Source, for_size, load};

/// Sizes of the Windows executable icon.
const ICO_SIZES: [u32; 8] = [16, 20, 24, 32, 40, 48, 64, 256];
/// Sizes of the macOS icon and of the PNG icons.
const ICNS_SIZES: [u32; 7] = [16, 32, 64, 128, 256, 512, 1024];
/// Sizes of the Windows tray glyphs.
const WIN_TRAY_SIZES: [u32; 4] = [16, 20, 24, 32];
/// Sizes of the Linux tray PNGs.
const LINUX_TRAY_SIZES: [u32; 2] = [22, 24];
/// Color of the light tray glyph on dark panels.
const WHITE: [u8; 3] = [255, 255, 255];

/// Renders `large` at `size` pixels, using the small variant where one exists.
fn render_at(src: &Path, large: Source, size: u32) -> Result<Pixmap> {
    let svg = load(src, for_size(large, size))?;
    render_svg(&svg, size)
}

/// Renders `large` at each of `sizes`.
fn render_all(src: &Path, large: Source, sizes: &[u32]) -> Result<Vec<Pixmap>> {
    sizes.iter().map(|&s| render_at(src, large, s)).collect()
}

/// Creates `dir` and its parents.
fn mkdir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))
}

/// Writes the desktop icons into `out`.
///
/// Produces `desktop/eagle.ico`, `desktop/eagle.icns` and `png/icon-{size}.png`.
pub fn build_desktop(src: &Path, out: &Path) -> Result<()> {
    let desktop = out.join("desktop");
    let png_dir = out.join("png");
    mkdir(&desktop)?;
    mkdir(&png_dir)?;

    let sized = render_all(src, Source::Icon, &ICNS_SIZES)?;
    write_icns(&sized, &desktop.join("eagle.icns"))?;
    for p in &sized {
        write_png(p, &png_dir.join(format!("icon-{}.png", p.width())))?;
    }

    let ico_images = render_all(src, Source::Icon, &ICO_SIZES)?;
    write_ico(&ico_images, &desktop.join("eagle.ico"))
}

/// Writes the tray icons into `out`.
///
/// macOS gets black template PNGs. Windows gets a black and a white ICO.
/// Linux gets a symbolic SVG and white PNGs.
pub fn build_tray(src: &Path, out: &Path) -> Result<()> {
    let macos = out.join("tray/macos");
    mkdir(&macos)?;
    for (name, size) in [("trayTemplate.png", 18), ("trayTemplate@2x.png", 36)] {
        let p = render_at(src, Source::Tray, size)?;
        write_png(&to_template(&p), &macos.join(name))?;
    }

    let windows = out.join("tray/windows");
    mkdir(&windows)?;
    let glyphs = render_all(src, Source::Tray, &WIN_TRAY_SIZES)?;
    let light: Vec<Pixmap> = glyphs.iter().map(to_template).collect();
    write_ico(&light, &windows.join("tray-light.ico"))?;
    let dark: Vec<Pixmap> = glyphs.iter().map(|p| tint(p, WHITE)).collect();
    write_ico(&dark, &windows.join("tray-dark.ico"))?;

    let linux = out.join("tray/linux");
    mkdir(&linux)?;
    fs::copy(
        src.join(Source::Tray.file_name()),
        linux.join("tray-symbolic.svg"),
    )
    .context("failed to copy tray-symbolic.svg")?;
    for size in LINUX_TRAY_SIZES {
        let p = tint(&render_at(src, Source::Tray, size)?, WHITE);
        write_png(&p, &linux.join(format!("tray-{size}.png")))?;
    }
    Ok(())
}
