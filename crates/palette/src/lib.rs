// SPDX-License-Identifier: GPL-3.0-or-later
//! Bounded cover decoding, two persisted sizes and median-cut colors.
use image::{ImageReader, Limits, imageops::FilterType};
use std::{io::Cursor, path::Path, sync::Mutex};
pub const MAX_COVER_BYTES: usize = 2 * 1024 * 1024;
// Downloads may prepare disk thumbnails while the UI worker is active.
static DECODER: Mutex<()> = Mutex::new(());
fn cover_reader(bytes: &[u8]) -> Result<ImageReader<Cursor<&[u8]>>, Box<dyn std::error::Error>> {
    if bytes.len() > MAX_COVER_BYTES {
        return Err("cover exceeds byte limit".into());
    }
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(1200);
    limits.max_image_height = Some(1200);
    limits.max_alloc = Some(8 * 1024 * 1024);
    reader.limits(limits);
    Ok(reader)
}
/// Split the box with the largest channel spread, then take its weighted mean.
pub fn median_cut(pixels: &[[u8; 3]], count: usize) -> Vec<[u8; 3]> {
    if pixels.is_empty() || count == 0 {
        return vec![];
    }
    let spread = |p: &Vec<[u8; 3]>| {
        let mut ranges = [0u8; 3];
        for c in 0..3 {
            ranges[c] = p.iter().map(|v| v[c]).max().unwrap_or(0)
                - p.iter().map(|v| v[c]).min().unwrap_or(0);
        }
        let channel = (0..3).max_by_key(|&c| ranges[c]).unwrap_or(0);
        (ranges[channel], channel)
    };
    let mut boxes = vec![pixels.to_vec()];
    while boxes.len() < count {
        let Some(i) = (0..boxes.len())
            .filter(|&i| boxes[i].len() > 1 && spread(&boxes[i]).0 > 0)
            .max_by_key(|&i| spread(&boxes[i]).0)
        else {
            break;
        };
        let channel = spread(&boxes[i]).1;
        boxes[i].sort_unstable_by_key(|v| v[channel]);
        let middle = boxes[i].len() / 2;
        let right = boxes[i].split_off(middle);
        boxes.push(right);
    }
    boxes
        .iter()
        .map(|b| {
            let mut mean = [0; 3];
            for c in 0..3 {
                mean[c] = (b.iter().map(|v| u64::from(v[c])).sum::<u64>() / b.len() as u64) as u8;
            }
            mean
        })
        .collect()
}
pub fn process(
    bytes: &[u8],
    small: &Path,
    large: &Path,
) -> Result<Vec<[u8; 3]>, Box<dyn std::error::Error>> {
    let _decoder = DECODER.lock().unwrap_or_else(|e| e.into_inner());
    let reader = cover_reader(bytes)?;
    let image = reader.decode()?;
    let pixels: Vec<_> = image
        .resize_exact(32, 32, FilterType::Triangle)
        .to_rgb8()
        .pixels()
        .map(|p| p.0)
        .collect();
    image.resize(64, 64, FilterType::Triangle).save(small)?;
    image.resize(600, 600, FilterType::Lanczos3).save(large)?;
    Ok(median_cut(&pixels, 5))
}
/// Stable local artwork when the provider has no cover.
pub fn placeholder(small: &Path, large: &Path) -> Result<Vec<[u8; 3]>, Box<dyn std::error::Error>> {
    let color = [96, 104, 112];
    image::RgbImage::from_pixel(64, 64, image::Rgb(color)).save(small)?;
    image::RgbImage::from_pixel(600, 600, image::Rgb(color)).save(large)?;
    Ok(vec![color])
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn separates_two_colors() {
        let colors = median_cut(&[[255, 0, 0], [255, 0, 0], [0, 0, 255], [0, 0, 255]], 2);
        assert!(colors.contains(&[255, 0, 0]));
        assert!(colors.contains(&[0, 0, 255]));
        assert!(median_cut(&[], 5).is_empty());
    }
}

/// Strength of cover tint in the player. The background remains the dominant color.
#[derive(Clone, Copy, Debug, Default)]
pub enum Tint {
    Off,
    #[default]
    Soft,
    Bright,
}
#[derive(Clone, Copy, Debug)]
pub struct PlayerColors {
    pub background: [u8; 3],
    pub text: [u8; 3],
    pub secondary: [u8; 3],
    /// Hairline and progress track, a step from the surface towards the cover.
    pub line: [u8; 3],
    /// Accent for the liked heart: the cover, darkened 35 % on light themes.
    pub accent: [u8; 3],
}
fn luminance(rgb: [u8; 3]) -> f64 {
    let linear = rgb.map(|c| {
        let c = f64::from(c) / 255.;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    });
    linear[0] * 0.2126 + linear[1] * 0.7152 + linear[2] * 0.0722
}
pub fn contrast(a: [u8; 3], b: [u8; 3]) -> f64 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}
/// Mix in encoded sRGB: `k` of the way from `cover` to `base`.
fn mix(cover: [u8; 3], base: [u8; 3], k: f64) -> [u8; 3] {
    std::array::from_fn(|i| (f64::from(cover[i]) * (1. - k) + f64::from(base[i]) * k).round() as u8)
}
/// Ink of the light and dark themes; the secondary ink is the same colour mixed with the cover.
const INK_DARK: [u8; 3] = [0xF7, 0xF6, 0xF3];
const INK_PAPER: [u8; 3] = [0x15, 0x15, 0x17];
const WHITE: [u8; 3] = [255; 3];
/// Bar tint of the player footer, by level: Off keeps the theme background.
fn bar_mix(tint: Tint) -> f64 {
    match tint {
        Tint::Off => 1.,
        Tint::Soft => 0.94,
        Tint::Bright => 0.88,
    }
}
/// The larger now-playing panel uses its own tint levels from the desktop design.
fn panel_mix(tint: Tint, background: [u8; 3], light: bool) -> f64 {
    match tint {
        Tint::Off => 1.,
        Tint::Soft if light => 0.86,
        Tint::Bright if light => 0.70,
        Tint::Soft if background == [0; 3] => 0.84,
        Tint::Bright if background == [0; 3] => 0.66,
        Tint::Soft => 0.78,
        Tint::Bright => 0.60,
    }
}
/// Black or white, whichever reads better on `bar`.
fn readable_ink(bar: [u8; 3]) -> [u8; 3] {
    if contrast(bar, WHITE) >= contrast(bar, [0; 3]) {
        WHITE
    } else {
        [0; 3]
    }
}
/// Fades from `ink` towards `bar` until the secondary text reads on it.
fn fade_to_readable(bar: [u8; 3], ink: [u8; 3]) -> [u8; 3] {
    (0..=100)
        .rev()
        .map(|step| {
            let fade = f64::from(step) / 100.;
            std::array::from_fn(|i| {
                (f64::from(bar[i]) * fade + f64::from(ink[i]) * (1. - fade)).round() as u8
            })
        })
        .find(|&c| contrast(bar, c) >= 4.5)
        .unwrap_or(ink)
}
/// Builds one player surface from the mockup: the cover mixed into `base` at `k`, the
/// theme ink for text and the cover-tinted secondary ink. A colour that would fail
/// WCAG AA falls back to a readable ink.
fn themed_colors(cover: [u8; 3], base: [u8; 3], k: f64, light: bool) -> PlayerColors {
    let (ink, secondary_ink, secondary_weight, line_offset) = if light {
        (INK_PAPER, INK_PAPER, 0.25, 0.26)
    } else {
        (INK_DARK, WHITE, 0.3, 0.23)
    };
    let bar = mix(cover, base, k);
    let line = mix(cover, base, (k - line_offset).max(0.3));
    let text = if contrast(bar, ink) >= 4.5 {
        ink
    } else {
        readable_ink(bar)
    };
    let tinted = mix(secondary_ink, cover, secondary_weight);
    let secondary = if contrast(bar, tinted) >= 4.5 {
        tinted
    } else {
        fade_to_readable(bar, text)
    };
    PlayerColors {
        background: bar,
        text,
        secondary,
        line,
        accent: if light {
            mix(cover, [0; 3], 0.35)
        } else {
            cover
        },
    }
}
/// Footer bar colours for the theme background.
pub fn player_colors(cover: [u8; 3], background: [u8; 3], tint: Tint) -> PlayerColors {
    themed_colors(
        cover,
        background,
        bar_mix(tint),
        luminance(background) > 0.5,
    )
}
/// The larger now-playing panel. With the tint off it sits on the theme surface.
pub fn panel_colors(
    cover: [u8; 3],
    background: [u8; 3],
    surface: [u8; 3],
    tint: Tint,
) -> PlayerColors {
    let light = luminance(background) > 0.5;
    let base = if matches!(tint, Tint::Off) {
        surface
    } else {
        background
    };
    themed_colors(cover, base, panel_mix(tint, background, light), light)
}
/// Use a single readable ink pair on both player surfaces.
pub fn player_surfaces(
    cover: [u8; 3],
    background: [u8; 3],
    surface: [u8; 3],
    tint: Tint,
) -> (PlayerColors, PlayerColors) {
    let mut bar = player_colors(cover, background, tint);
    let mut panel = panel_colors(cover, background, surface, tint);
    let secondary = [panel.secondary, bar.secondary, panel.text]
        .into_iter()
        .find(|&color| {
            contrast(bar.background, color) >= 4.5 && contrast(panel.background, color) >= 4.5
        })
        .unwrap_or(panel.text);
    bar.secondary = secondary;
    panel.secondary = secondary;
    (bar, panel)
}
/// Colours of the wave screen: the mood pad, its labels and the card of the current track.
#[derive(Clone, Copy, Debug)]
pub struct WavePad {
    pub background: [u8; 3],
    /// The thin ring that marks the reach of the knob.
    pub ring: [u8; 3],
    /// Highlighted label and ink on the card.
    pub text: [u8; 3],
    /// Labels, readable on `background` and on `card`.
    pub secondary: [u8; 3],
    /// The dots of the field, the knob and the filled part of the slider.
    pub dot: [u8; 3],
    pub card: [u8; 3],
}
/// Moves `color` toward `ink` until it reads on every surface in `surfaces`.
fn readable_on(color: [u8; 3], surfaces: &[[u8; 3]], ink: [u8; 3], minimum: f64) -> [u8; 3] {
    (0..=20)
        .map(|step| mix(color, ink, f64::from(step) / 20.))
        .find(|&candidate| {
            surfaces
                .iter()
                .all(|&surface| contrast(surface, candidate) >= minimum)
        })
        .unwrap_or(ink)
}
/// The wave pad takes the cover's colour; the theme background stays the dominant colour.
/// Mixed in encoded sRGB; text roles keep WCAG AA contrast on the pad and on the card.
pub fn wave_pad(cover: [u8; 3], background: [u8; 3]) -> WavePad {
    let light = luminance(background) > 0.5;
    if light {
        let pad = mix(cover, background, 0.88);
        let card = mix(cover, background, 0.94);
        let ink = [0; 3];
        WavePad {
            background: pad,
            ring: mix(cover, background, 0.74),
            text: readable_on(mix(cover, ink, 0.45), &[pad, card], ink, 7.),
            secondary: readable_on(mix(cover, background, 0.38), &[pad, card], ink, 4.5),
            dot: mix(cover, ink, 0.2),
            card,
        }
    } else {
        let k = if background == [0; 3] { 0.86 } else { 0.8 };
        let pad = mix(cover, background, k);
        let card = mix(cover, background, 0.9);
        let ink = [255; 3];
        WavePad {
            background: pad,
            ring: mix(cover, background, 0.66),
            text: readable_on(mix(ink, cover, 0.15), &[pad, card], ink, 7.),
            secondary: readable_on(mix(cover, ink, 0.25), &[pad, card], ink, 4.5),
            dot: mix(cover, ink, 0.45),
            card,
        }
    }
}
#[cfg(test)]
mod wave_pad_tests {
    use super::*;
    #[test]
    fn labels_stay_readable_on_the_pad_and_the_card_for_every_cover_and_theme() {
        for background in [[18, 18, 20], [250, 250, 247], [0; 3]] {
            for r in (0..=255).step_by(17) {
                for g in (0..=255).step_by(17) {
                    for b in (0..=255).step_by(17) {
                        let pad = wave_pad([r, g, b], background);
                        for surface in [pad.background, pad.card] {
                            assert!(contrast(surface, pad.text) >= 4.5);
                            assert!(contrast(surface, pad.secondary) >= 4.5);
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn the_pad_is_a_light_wash_of_the_cover_on_paper_and_dark_on_graphite() {
        let cover = [38, 70, 83];
        let paper = wave_pad(cover, [250, 250, 247]);
        let graphite = wave_pad(cover, [18, 18, 20]);
        assert!(luminance(paper.background) > 0.5);
        assert!(luminance(graphite.background) < 0.2);
        assert_ne!(paper.background, [250, 250, 247]);
    }
}
#[cfg(test)]
mod contrast_tests {
    use super::*;
    #[test]
    fn all_themes_and_tints_keep_both_text_roles_readable() {
        let themes = [
            ([18, 18, 20], [26, 26, 29]),
            ([250, 250, 247], [241, 241, 236]),
            ([0; 3], [14, 14, 17]),
        ];
        for (background, surface) in themes {
            for tint in [Tint::Off, Tint::Soft, Tint::Bright] {
                for r in (0..=255).step_by(17) {
                    for g in (0..=255).step_by(17) {
                        for b in (0..=255).step_by(17) {
                            let (colors, panel) =
                                player_surfaces([r, g, b], background, surface, tint);
                            assert!(contrast(colors.background, colors.text) >= 4.5);
                            assert!(contrast(colors.background, colors.secondary) >= 4.5);
                            assert!(contrast(panel.background, panel.text) >= 4.5);
                            assert!(contrast(panel.background, panel.secondary) >= 4.5);
                            assert!(contrast(colors.background, panel.secondary) >= 4.5);
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn dark_and_paper_inks_follow_the_design() {
        let cover = [0x26, 0x46, 0x53];
        let dark = player_colors(cover, [18, 18, 20], Tint::Off);
        assert_eq!(dark.text, INK_DARK);
        let paper = player_colors(cover, [250, 250, 247], Tint::Off);
        assert_eq!(paper.text, INK_PAPER);
        assert_eq!(paper.secondary, mix(INK_PAPER, cover, 0.25));
    }
    #[test]
    fn panel_with_tint_off_sits_on_the_surface() {
        let surface = [26, 26, 29];
        let panel = panel_colors([38, 70, 83], [18, 18, 20], surface, Tint::Off);
        assert_eq!(panel.background, surface);
        let bar = player_colors([38, 70, 83], [18, 18, 20], Tint::Off);
        assert_eq!(bar.background, [18, 18, 20]);
    }
}

/// A bounded, display-sized cover that is safe to transfer to the UI thread.
pub struct Cover {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub dominant: [u8; 3],
}
pub fn decode_cover(bytes: &[u8], size: u32) -> Result<Cover, Box<dyn std::error::Error>> {
    if !(1..=1200).contains(&size) {
        return Err("invalid cover size".into());
    }
    let _decoder = DECODER.lock().unwrap_or_else(|e| e.into_inner());
    let reader = cover_reader(bytes)?;
    let decoded = reader.decode()?;
    let samples: Vec<_> = decoded
        .resize_exact(32, 32, FilterType::Triangle)
        .to_rgb8()
        .pixels()
        .map(|p| p.0)
        .collect();
    let dominant = median_cut(&samples, 1).first().copied().unwrap_or([128; 3]);
    let image = decoded.resize(size, size, FilterType::Triangle).to_rgba8();
    Ok(Cover {
        width: image.width(),
        height: image.height(),
        rgba: image.into_raw(),
        dominant,
    })
}

#[cfg(test)]
mod limit_tests {
    use super::*;
    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut output = Cursor::new(Vec::new());
        image::RgbImage::new(width, height)
            .write_to(&mut output, image::ImageFormat::Png)
            .unwrap();
        output.into_inner()
    }
    #[test]
    fn rejects_corruption_compressed_limit_and_oversized_dimensions() {
        assert!(decode_cover(b"broken", 64).is_err());
        assert!(decode_cover(&vec![0; MAX_COVER_BYTES + 1], 64).is_err());
        assert!(decode_cover(&png(1201, 1), 64).is_err());
        let cover = decode_cover(&png(1200, 1200), 600).unwrap();
        assert_eq!(cover.rgba.len(), 600 * 600 * 4);
        assert!(decode_cover(&png(1, 1), 1201).is_err());
    }
}
