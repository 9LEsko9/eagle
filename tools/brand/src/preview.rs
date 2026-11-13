// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Writes `preview.png`, a sheet that shows the brand assets at their real sizes.

use std::path::Path;

use anyhow::{Context, Result};
use resvg::tiny_skia::{Color, Pixmap, PixmapPaint, PremultipliedColorU8, Transform};

use crate::pack::write_png;
use crate::palette::{DISC, rgb};
use crate::render::{recolor_background, render_svg, tint, to_template};
use crate::sources::{Source, for_size, load};

/// Width of the sheet in pixels.
const WIDTH: u32 = 1600;
/// Height of the sheet in pixels.
const HEIGHT: u32 = 1000;
/// Background of the sheet.
const PAPER: &str = "#FAFAF7";
/// Dark app icon background, also used for the dark mark panel.
const DARK_PANEL: &str = "#1A1A1D";
/// Recolored app icon backgrounds, shown at 128 px.
const ICON_BACKGROUNDS: [&str; 3] = ["#3E2430", "#1F3A39", "#22304A"];
/// Light tray panel, where the black template glyphs are shown.
const TRAY_LIGHT_PANEL: &str = "#E8E8E8";
/// Dark tray panel, where the white glyphs are shown.
const TRAY_DARK_PANEL: &str = "#2B2B2E";
/// Dark Windows tray panel.
const TRAY_WINDOWS_PANEL: &str = "#1C1C1C";
/// Color of the white glyphs on dark panels.
const WHITE: [u8; 3] = [255, 255, 255];
/// Side of each Android tile, in pixels.
const ANDROID_TILE: u32 = 216;
/// Gap between neighbouring tiles, in pixels.
const GAP: i32 = 48;
/// Left margin of the sheet.
const LEFT: i32 = 60;
/// Top margin of the sheet.
const TOP: i32 = 60;
/// Left edge of the right-hand column, which holds the mark.
const RIGHT_COLUMN: i32 = 860;

/// Renders `preview.png` into `out`, showing the assets from `src`.
pub fn write_sheet(src: &Path, out: &Path) -> Result<()> {
    let mut sheet = Pixmap::new(WIDTH, HEIGHT).context("pixmap size must be non-zero")?;
    sheet.fill(color(PAPER));

    draw_icon_sizes(src, &mut sheet)?;
    draw_icon_backgrounds(src, &mut sheet)?;
    draw_android(src, &mut sheet)?;
    draw_trays(src, &mut sheet)?;
    draw_marks(src, &mut sheet)?;

    write_png(&sheet, &out.join("preview.png"))
}

/// Converts a `#RRGGBB` brand color to an opaque tiny-skia color.
fn color(hex: &str) -> Color {
    let [r, g, b] = rgb(hex);
    Color::from_rgba8(r, g, b, u8::MAX)
}

/// Returns a `w` x `h` pixmap filled with `hex`.
fn panel(w: u32, h: u32, hex: &str) -> Result<Pixmap> {
    let mut p = Pixmap::new(w, h).context("pixmap size must be non-zero")?;
    p.fill(color(hex));
    Ok(p)
}

/// Draws `p` on `sheet` with its top left corner at `(x, y)`.
fn place(sheet: &mut Pixmap, p: &Pixmap, x: i32, y: i32) {
    sheet.draw_pixmap(
        x,
        y,
        p.as_ref(),
        &PixmapPaint::default(),
        Transform::identity(),
        None,
    );
}

/// Renders `large` at `size` pixels, using the small variant where one exists.
fn icon(src: &Path, large: Source, size: u32) -> Result<Pixmap> {
    render_svg(&load(src, for_size(large, size))?, size)
}

/// Clears every pixel of `p` outside the given shape.
#[derive(Clone, Copy)]
enum Shape {
    Circle,
    /// A circle with a square corner in its bottom-right quadrant.
    Teardrop,
}

/// Makes the pixels of `p` outside `shape` fully transparent.
fn mask(p: &mut Pixmap, shape: Shape) {
    let w = p.width();
    let radius = w as f32 / 2.0;
    for (i, px) in p.pixels_mut().iter_mut().enumerate() {
        let x = (i as u32 % w) as f32 + 0.5 - radius;
        let y = (i as u32 / w) as f32 + 0.5 - radius;
        let in_circle = x * x + y * y <= radius * radius;
        let keep = match shape {
            Shape::Circle => in_circle,
            Shape::Teardrop => in_circle || (x >= 0.0 && y >= 0.0),
        };
        if !keep {
            *px = PremultipliedColorU8::TRANSPARENT;
        }
    }
}

/// Row 1: the full icon at 256, 128, 64, 32, 24 and 16 px.
fn draw_icon_sizes(src: &Path, sheet: &mut Pixmap) -> Result<()> {
    let mut x = LEFT;
    for size in [256, 128, 64, 32, 24, 16] {
        place(sheet, &icon(src, Source::Icon, size)?, x, TOP);
        x += size as i32 + GAP;
    }
    Ok(())
}

/// Row 2: the full icon at 128 px on three recolored backgrounds.
fn draw_icon_backgrounds(src: &Path, sheet: &mut Pixmap) -> Result<()> {
    let base = load(src, Source::Icon)?;
    let y = 340;
    for (i, hex) in ICON_BACKGROUNDS.iter().enumerate() {
        let svg = recolor_background(&base, hex);
        let p = render_svg(&svg, 128)?;
        place(sheet, &p, LEFT + i as i32 * (128 + GAP), y);
    }
    Ok(())
}

/// Row 3: the Android foreground over the background, unmasked and masked.
fn draw_android(src: &Path, sheet: &mut Pixmap) -> Result<()> {
    let background = load(src, Source::AndroidBg)?;
    let foreground = load(src, Source::AndroidFg)?;
    let y = 520;
    for (i, shape) in [None, Some(Shape::Circle), Some(Shape::Teardrop)]
        .into_iter()
        .enumerate()
    {
        let mut tile = render_svg(&background, ANDROID_TILE)?;
        let fg = render_svg(&foreground, ANDROID_TILE)?;
        place(&mut tile, &fg, 0, 0);
        if let Some(shape) = shape {
            mask(&mut tile, shape);
        }
        place(
            sheet,
            &tile,
            LEFT + i as i32 * (ANDROID_TILE as i32 + GAP),
            y,
        );
    }
    Ok(())
}

/// Row 4: macOS template tray glyphs and Windows tray glyphs on their panels.
fn draw_trays(src: &Path, sheet: &mut Pixmap) -> Result<()> {
    let y = 790;
    let (panel_w, panel_h) = (480_i32, 110_i32);
    let light = panel(panel_w as u32, panel_h as u32, TRAY_LIGHT_PANEL)?;
    let dark = panel(panel_w as u32, panel_h as u32, TRAY_DARK_PANEL)?;
    let windows = panel(panel_w as u32, panel_h as u32, TRAY_WINDOWS_PANEL)?;

    let mut black = light;
    let mut white = dark;
    let mut win = windows;
    let mut x = 20;
    for size in [18, 36] {
        let glyph = icon(src, Source::Tray, size)?;
        let dy = (panel_h - size as i32) / 2;
        place(&mut black, &to_template(&glyph), x, dy);
        place(&mut white, &tint(&glyph, WHITE), x, dy);
        x += size as i32 + 40;
    }
    let mut x = 20;
    for size in [16, 24] {
        let glyph = icon(src, Source::Tray, size)?;
        let dy = (panel_h - size as i32) / 2;
        place(&mut win, &tint(&glyph, WHITE), x, dy);
        x += size as i32 + 40;
    }

    place(sheet, &black, LEFT, y);
    place(sheet, &white, LEFT + panel_w + 20, y);
    place(sheet, &win, LEFT + 2 * (panel_w + 20), y);
    Ok(())
}

/// Row 5: the mark at 256 and 32 px on light and dark backgrounds.
///
/// The mark has no dark variant in the sources, so the dark panels show it
/// with the head and eye inverted. This affects the preview only.
fn draw_marks(src: &Path, sheet: &mut Pixmap) -> Result<()> {
    let svg = load(src, Source::Mark)?;
    let inverted = invert_mark(&svg);
    let mark = |size| render_svg(&svg, size);
    let inverted_mark = |size| render_svg(&inverted, size);

    place(sheet, &mark(256)?, RIGHT_COLUMN, TOP);
    let mut dark_large = panel(256, 256, DARK_PANEL)?;
    place(&mut dark_large, &inverted_mark(256)?, 0, 0);
    place(sheet, &dark_large, RIGHT_COLUMN + 256 + GAP, TOP);

    let y = 340;
    place(sheet, &mark(32)?, RIGHT_COLUMN, y);
    let mut dark_small = panel(64, 64, DARK_PANEL)?;
    place(&mut dark_small, &inverted_mark(32)?, 16, 16);
    place(sheet, &dark_small, RIGHT_COLUMN + 32 + GAP, y);
    Ok(())
}

/// Swaps the head and eye colors of the mark, so it reads on a dark panel.
fn invert_mark(svg: &str) -> String {
    let eye = format!("id=\"eye\" fill=\"{DISC}\"");
    let inverted_eye = format!("id=\"eye\" fill=\"{DARK_PANEL}\"");
    recolor_background(svg, DISC).replace(&eye, &inverted_eye)
}
