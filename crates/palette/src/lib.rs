// SPDX-License-Identifier: GPL-3.0-or-later
//! Bounded cover decoding, two persisted sizes and median-cut colors.
use image::{ImageReader, Limits, imageops::FilterType};
use std::{io::Cursor, path::Path};
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
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
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
/// Mix in encoded sRGB, then choose readable ink for both text roles (WCAG AA).
pub fn player_colors(cover: [u8; 3], background: [u8; 3], tint: Tint) -> PlayerColors {
    let k = match tint {
        Tint::Off => 1.,
        Tint::Soft => 0.94,
        Tint::Bright => 0.88,
    };
    colors_with_mix(cover, background, k)
}
/// The larger now-playing panel uses the tint levels from the desktop design.
pub fn panel_colors(cover: [u8; 3], background: [u8; 3], tint: Tint) -> PlayerColors {
    let k = match tint {
        Tint::Off => 1.,
        Tint::Soft if luminance(background) > 0.5 => 0.86,
        Tint::Bright if luminance(background) > 0.5 => 0.70,
        Tint::Soft if background == [0; 3] => 0.84,
        Tint::Bright if background == [0; 3] => 0.66,
        Tint::Soft => 0.78,
        Tint::Bright => 0.60,
    };
    colors_with_mix(cover, background, k)
}
/// Use a single readable ink pair on both player surfaces.
pub fn player_surfaces(
    cover: [u8; 3],
    background: [u8; 3],
    tint: Tint,
) -> (PlayerColors, PlayerColors) {
    let mut bar = player_colors(cover, background, tint);
    let mut panel = panel_colors(cover, background, tint);
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
fn colors_with_mix(cover: [u8; 3], background: [u8; 3], k: f64) -> PlayerColors {
    let bar = std::array::from_fn(|i| {
        (f64::from(cover[i]) * (1. - k) + f64::from(background[i]) * k).round() as u8
    });
    let text = if contrast(background, [255; 3]) >= contrast(background, [0; 3]) {
        [255; 3]
    } else {
        [0; 3]
    };
    let secondary = (0..=100)
        .rev()
        .map(|step| {
            let fade = f64::from(step) / 100.;
            std::array::from_fn(|i| {
                (f64::from(bar[i]) * fade + f64::from(text[i]) * (1. - fade)).round() as u8
            })
        })
        .find(|&c| contrast(bar, c) >= 4.5)
        .unwrap_or(text);
    PlayerColors {
        background: bar,
        text,
        secondary,
    }
}
#[cfg(test)]
mod contrast_tests {
    use super::*;
    #[test]
    fn all_themes_and_tints_keep_both_text_roles_readable() {
        for background in [[18, 18, 20], [250, 250, 247], [0; 3]] {
            for tint in [Tint::Off, Tint::Soft, Tint::Bright] {
                for r in (0..=255).step_by(17) {
                    for g in (0..=255).step_by(17) {
                        for b in (0..=255).step_by(17) {
                            let (colors, panel) = player_surfaces([r, g, b], background, tint);
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
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
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
