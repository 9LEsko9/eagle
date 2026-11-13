// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Brand SVG sources and the choice of variant for a target size.

use std::path::Path;

use anyhow::{Context, Result};

/// Largest target size, in pixels, that uses the small variant of an icon.
pub const SMALL_MAX: u32 = 32;

/// A brand SVG source file in `assets/brand/src`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Icon,
    IconSmall,
    Mark,
    Tray,
    TraySmall,
    AndroidFg,
    AndroidBg,
    AndroidMono,
}

/// Every source, in declaration order.
pub const ALL_SOURCES: [Source; 8] = [
    Source::Icon,
    Source::IconSmall,
    Source::Mark,
    Source::Tray,
    Source::TraySmall,
    Source::AndroidFg,
    Source::AndroidBg,
    Source::AndroidMono,
];

impl Source {
    /// Returns the file name of this source inside the source directory.
    pub fn file_name(self) -> &'static str {
        match self {
            Source::Icon => "icon.svg",
            Source::IconSmall => "icon-small.svg",
            Source::Mark => "mark.svg",
            Source::Tray => "tray.svg",
            Source::TraySmall => "tray-small.svg",
            Source::AndroidFg => "android-fg.svg",
            Source::AndroidBg => "android-bg.svg",
            Source::AndroidMono => "android-mono.svg",
        }
    }
}

/// Picks the variant to render `large` with at `size` pixels.
///
/// The full icon and the tray icon switch to their small variants at
/// `SMALL_MAX` pixels and below; every other source is returned unchanged.
pub fn for_size(large: Source, size: u32) -> Source {
    match large {
        Source::Icon if size <= SMALL_MAX => Source::IconSmall,
        Source::Tray if size <= SMALL_MAX => Source::TraySmall,
        other => other,
    }
}

/// Reads the SVG text of `s` from `src_dir`.
pub fn load(src_dir: impl AsRef<Path>, s: Source) -> Result<String> {
    let path = src_dir.as_ref().join(s.file_name());
    std::fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))
}
