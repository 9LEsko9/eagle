// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

mod common;

use std::fs::File;
use std::path::Path;

use common::{SRC, build_tmp, read_png_rgba};
use eagle_brand::build::{build_desktop, build_tray};
use eagle_brand::pack::straight_rgba;
use eagle_brand::render::render_svg;
use eagle_brand::sources::{Source, load};

const PNG_SIZES: [u32; 7] = [16, 32, 64, 128, 256, 512, 1024];
const WIN_TRAY_SIZES: [u32; 4] = [16, 20, 24, 32];

/// Decodes every entry of the ICO at `path`, sorted by width.
fn ico_entries(path: &Path) -> Vec<ico::IconImage> {
    let dir = ico::IconDir::read(File::open(path).unwrap()).unwrap();
    let mut images: Vec<ico::IconImage> =
        dir.entries().iter().map(|e| e.decode().unwrap()).collect();
    images.sort_by_key(|i| i.width());
    images
}

#[test]
fn ico_has_all_sizes() {
    let dir = build_tmp(build_desktop);
    let icon_dir = ico::IconDir::read(File::open(dir.join("desktop/eagle.ico")).unwrap()).unwrap();
    let mut sizes: Vec<u32> = icon_dir.entries().iter().map(|e| e.width()).collect();
    sizes.sort();
    assert_eq!(sizes, vec![16, 20, 24, 32, 40, 48, 64, 256]);
}

#[test]
fn icns_has_all_sizes() {
    let dir = build_tmp(build_desktop);
    let fam = icns::IconFamily::read(File::open(dir.join("desktop/eagle.icns")).unwrap()).unwrap();
    for t in [
        icns::IconType::RGBA32_16x16,
        icns::IconType::RGBA32_32x32,
        icns::IconType::RGBA32_64x64,
        icns::IconType::RGBA32_128x128,
        icns::IconType::RGBA32_256x256,
        icns::IconType::RGBA32_512x512,
        icns::IconType::RGBA32_512x512_2x,
    ] {
        assert!(fam.has_icon_with_type(t), "{t:?}");
    }
}

#[test]
fn png_icons_have_all_sizes() {
    let dir = build_tmp(build_desktop);
    for s in PNG_SIZES {
        let img = read_png_rgba(&dir.join(format!("png/icon-{s}.png")));
        assert_eq!((img.width, img.height), (s, s));
    }
}

#[test]
fn small_ico_entries_use_small_source() {
    let dir = build_tmp(build_desktop);
    let images = ico_entries(&dir.join("desktop/eagle.ico"));
    let entry = images.iter().find(|i| i.width() == 16).unwrap();

    let small = straight_rgba(&render_svg(&load(SRC, Source::IconSmall).unwrap(), 16).unwrap());
    let full = straight_rgba(&render_svg(&load(SRC, Source::Icon).unwrap(), 16).unwrap());
    assert_ne!(small, full, "the two 16 px variants must differ");
    assert_eq!(entry.rgba_data(), small.as_slice());
}

#[test]
fn macos_tray_is_template() {
    let dir = build_tmp(build_tray);
    for (f, s) in [("trayTemplate.png", 18), ("trayTemplate@2x.png", 36)] {
        let img = read_png_rgba(&dir.join("tray/macos").join(f));
        assert_eq!((img.width, img.height), (s, s));
        assert!(img.pixels().all(|[r, g, b, _]| r == 0 && g == 0 && b == 0));
    }
}

#[test]
fn windows_tray_glyphs_are_black_and_white() {
    let dir = build_tmp(build_tray);
    for (file, rgb) in [("tray-light.ico", 0), ("tray-dark.ico", 255)] {
        let images = ico_entries(&dir.join("tray/windows").join(file));
        let sizes: Vec<u32> = images.iter().map(|i| i.width()).collect();
        assert_eq!(sizes, WIN_TRAY_SIZES, "{file}");
        for img in &images {
            let glyph: Vec<[u8; 4]> = img
                .rgba_data()
                .as_chunks::<4>()
                .0
                .iter()
                .copied()
                .filter(|p| p[3] > 0)
                .collect();
            assert!(!glyph.is_empty(), "{file} {}px is empty", img.width());
            assert!(
                glyph
                    .iter()
                    .all(|p| p[0] == rgb && p[1] == rgb && p[2] == rgb),
                "{file} {}px",
                img.width()
            );
        }
    }
}

#[test]
fn linux_tray_assets_are_symbolic_and_white() {
    let dir = build_tmp(build_tray);
    let svg = dir.join("tray/linux/tray-symbolic.svg");
    assert_eq!(
        std::fs::read(&svg).unwrap(),
        std::fs::read(Path::new(SRC).join("tray.svg")).unwrap()
    );
    for s in [22, 24] {
        let img = read_png_rgba(&dir.join(format!("tray/linux/tray-{s}.png")));
        assert_eq!((img.width, img.height), (s, s));
        assert!(
            img.pixels()
                .filter(|p| p[3] > 0)
                .all(|[r, g, b, _]| r == 255 && g == 255 && b == 255)
        );
    }
}

#[test]
fn build_is_deterministic() {
    let a = build_tmp(build_desktop);
    let b = build_tmp(build_desktop);
    for f in [
        "desktop/eagle.ico",
        "desktop/eagle.icns",
        "png/icon-1024.png",
    ] {
        assert_eq!(
            std::fs::read(a.join(f)).unwrap(),
            std::fs::read(b.join(f)).unwrap(),
            "{f}"
        );
    }
}
