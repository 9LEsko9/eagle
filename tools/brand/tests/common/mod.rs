// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared helpers for the build tests.

use std::collections::hash_map::DefaultHasher;
use std::fs::{self, File};
use std::hash::Hasher;
use std::io::BufReader;
use std::ops::Deref;
use std::path::{Path, PathBuf};

use anyhow::Result;
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

/// Lists every file under `dir` as `(relative path, content hash)`, sorted by path.
///
/// The hash uses `DefaultHasher`, which is deterministic within one process.
#[allow(dead_code)]
pub fn hash_tree(dir: &Path) -> Vec<(String, u64)> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let entries = fs::read_dir(&d).unwrap_or_else(|e| panic!("read {}: {e}", d.display()));
        for entry in entries {
            let path = entry.expect("failed to read dir entry").path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let rel = path
                .strip_prefix(dir)
                .expect("file is under the tree root")
                .to_string_lossy()
                .into_owned();
            let bytes = fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            let mut hasher = DefaultHasher::new();
            hasher.write(&bytes);
            files.push((rel, hasher.finish()));
        }
    }
    files.sort();
    files
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
