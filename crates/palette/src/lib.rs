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
