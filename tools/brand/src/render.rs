// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! SVG rendering and recoloring helpers.

use anyhow::{Context, Result, bail};
use resvg::tiny_skia::{ColorU8, Pixmap, Transform};
use resvg::usvg;

use crate::palette::GRAPHITE;

/// Renders `svg` onto a `size` x `size` pixmap.
///
/// The whole image is scaled uniformly to fit the square and centered on it.
pub fn render_svg(svg: &str, size: u32) -> Result<Pixmap> {
    let tree =
        usvg::Tree::from_str(svg, &usvg::Options::default()).context("failed to parse SVG")?;
    let mut pixmap = Pixmap::new(size, size).context("pixmap size must be non-zero")?;

    let image = tree.size();
    let (w, h) = (image.width(), image.height());
    let scale = size as f32 / w.max(h);
    if !scale.is_finite() || scale <= 0.0 {
        bail!("SVG has an empty or invalid size");
    }
    let tx = (size as f32 - w * scale) / 2.0;
    let ty = (size as f32 - h * scale) / 2.0;
    let transform = Transform::from_row(scale, 0.0, 0.0, scale, tx, ty);

    resvg::render(&tree, transform, &mut pixmap.as_mut());
    Ok(pixmap)
}

/// Replaces the background color `GRAPHITE` in `svg` with `hex`.
///
/// Both the upper-case and lower-case spellings are replaced.
pub fn recolor_background(svg: &str, hex: &str) -> String {
    svg.replace(GRAPHITE, hex)
        .replace(&GRAPHITE.to_ascii_lowercase(), hex)
}

/// Turns every pixel into black with the same alpha, for macOS template images.
pub fn to_template(p: &Pixmap) -> Pixmap {
    map_pixels(p, |a| ColorU8::from_rgba(0, 0, 0, a))
}

/// Sets every pixel to `rgb` while keeping its alpha.
pub fn tint(p: &Pixmap, rgb: [u8; 3]) -> Pixmap {
    map_pixels(p, |a| ColorU8::from_rgba(rgb[0], rgb[1], rgb[2], a))
}

/// Builds a pixmap of the same size as `p` where each pixel is `f(alpha)`.
fn map_pixels(p: &Pixmap, f: impl Fn(u8) -> ColorU8) -> Pixmap {
    let mut out = p.clone();
    for (dst, src) in out.pixels_mut().iter_mut().zip(p.pixels()) {
        *dst = f(src.alpha()).premultiply();
    }
    out
}
