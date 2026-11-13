// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared helpers for the build tests.

use std::fs::File;
use std::io::BufReader;
use std::ops::Deref;
use std::path::{Path, PathBuf};

use anyhow::Result;
use resvg::tiny_skia::Pixmap;
use tempfile::TempDir;

/// Brand SVG sources, relative to this crate.
pub const SRC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/brand/src");

/// Output of a build run into a temporary directory.
///
/// Derefs to the output directory, and removes it when dropped.
pub struct Built {
    _dir: TempDir,
    out: PathBuf,
}

impl Deref for Built {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.out
    }
}

/// Runs `build` on `SRC` into a fresh temporary directory.
#[allow(dead_code)]
pub fn build_tmp(build: fn(&Path, &Path) -> Result<()>) -> Built {
    let dir = TempDir::new().expect("failed to create temp dir");
    let out = dir.path().to_path_buf();
    build(Path::new(SRC), &out).unwrap_or_else(|e| panic!("build failed: {e:?}"));
    Built { _dir: dir, out }
}

/// A decoded RGBA8 PNG.
#[allow(dead_code)]
pub struct Rgba {
    pub width: u32,
    pub height: u32,
    data: Vec<u8>,
}

impl Rgba {
    /// Iterates over the pixels in row-major order as `[r, g, b, a]`.
    #[allow(dead_code)]
    pub fn pixels(&self) -> impl Iterator<Item = [u8; 4]> + '_ {
        self.data.as_chunks::<4>().0.iter().copied()
    }
}

/// Decodes the RGBA8 PNG at `path`.
#[allow(dead_code)]
pub fn read_png_rgba(path: &Path) -> Rgba {
    let file = File::open(path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    let mut reader = png::Decoder::new(BufReader::new(file))
        .read_info()
        .unwrap_or_else(|e| panic!("decode {}: {e}", path.display()));
    let mut data = vec![0; reader.output_buffer_size().expect("PNG too large")];
    let info = reader
        .next_frame(&mut data)
        .unwrap_or_else(|e| panic!("decode {}: {e}", path.display()));
    assert_eq!(info.color_type, png::ColorType::Rgba, "{}", path.display());
    data.truncate(info.buffer_size());
    Rgba {
        width: info.width,
        height: info.height,
        data,
    }
}

/// Converts a premultiplied pixmap to straight (non-premultiplied) RGBA bytes.
#[allow(dead_code)]
pub fn straight_rgba(p: &Pixmap) -> Vec<u8> {
    p.pixels()
        .iter()
        .flat_map(|c| {
            let c = c.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect()
}
