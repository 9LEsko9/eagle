// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

mod common;

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use common::{SRC, build_tmp, read_png_rgba};
use eagle_brand::build::{build_android, build_ios, build_web};

const ANDROID_DENSITIES: [(&str, u32, u32); 5] = [
    ("mdpi", 108, 48),
    ("hdpi", 162, 72),
    ("xhdpi", 216, 96),
    ("xxhdpi", 324, 144),
    ("xxxhdpi", 432, 192),
];

const CONTENTS_JSON: &str = "{
  \"images\": [
    {
      \"idiom\": \"universal\",
      \"platform\": \"ios\",
      \"size\": \"1024x1024\",
      \"filename\": \"AppIcon-1024.png\"
    }
  ],
  \"info\": {
    \"author\": \"eagle-brand\",
    \"version\": 1
  }
}
";

/// Reads every file under `dir`, keyed by its path relative to `dir`.
fn tree(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let rel = path.strip_prefix(dir).unwrap().to_path_buf();
                out.insert(rel, fs::read(&path).unwrap());
            }
        }
    }
    out
}

#[test]
fn ios_icon_is_opaque() {
    let dir = build_tmp(build_ios);
    let img = read_png_rgba(&dir.join("ios/AppIcon.appiconset/AppIcon-1024.png"));
    assert_eq!((img.width, img.height), (1024, 1024));
    assert!(img.pixels().all(|[_, _, _, a]| a == 255));
}

#[test]
fn ios_contents_json_is_exact() {
    let dir = build_tmp(build_ios);
    let json = fs::read_to_string(dir.join("ios/AppIcon.appiconset/Contents.json")).unwrap();
    assert_eq!(json, CONTENTS_JSON);
}

#[test]
fn android_layers_per_density() {
    let dir = build_tmp(build_android);
    for (d, s, _) in ANDROID_DENSITIES {
        for layer in ["foreground", "background", "monochrome"] {
            let img =
                read_png_rgba(&dir.join(format!("android/res/mipmap-{d}/ic_launcher_{layer}.png")));
            assert_eq!((img.width, img.height), (s, s), "{d} {layer}");
        }
    }
    let xml =
        fs::read_to_string(dir.join("android/res/mipmap-anydpi-v26/ic_launcher.xml")).unwrap();
    for tag in ["<background", "<foreground", "<monochrome"] {
        assert!(xml.contains(tag), "{tag}");
    }
}

#[test]
fn android_adaptive_xml_references_layers() {
    let dir = build_tmp(build_android);
    let xml =
        fs::read_to_string(dir.join("android/res/mipmap-anydpi-v26/ic_launcher.xml")).unwrap();
    assert!(xml.contains("<adaptive-icon"));
    for layer in ["background", "foreground", "monochrome"] {
        let reference = format!("@mipmap/ic_launcher_{layer}");
        assert!(xml.contains(&reference), "{reference}");
    }
}

#[test]
fn android_legacy_icons_per_density() {
    let dir = build_tmp(build_android);
    for (d, _, s) in ANDROID_DENSITIES {
        let img = read_png_rgba(&dir.join(format!("android/res/mipmap-{d}/ic_launcher.png")));
        assert_eq!((img.width, img.height), (s, s), "{d}");
    }
}

#[test]
fn web_assets_exist() {
    let dir = build_tmp(build_web);
    for f in [
        "web/favicon.svg",
        "web/favicon.ico",
        "web/apple-touch-icon.png",
        "mark/mark-512.png",
    ] {
        assert!(dir.join(f).exists(), "{f}");
    }
}

#[test]
fn favicon_svg_is_the_mark() {
    let dir = build_tmp(build_web);
    let copy = fs::read(dir.join("web/favicon.svg")).unwrap();
    let mark = fs::read(Path::new(SRC).join("mark.svg")).unwrap();
    assert_eq!(copy, mark);
}

#[test]
fn favicon_ico_has_16_32_48() {
    let dir = build_tmp(build_web);
    let icon_dir = ico::IconDir::read(File::open(dir.join("web/favicon.ico")).unwrap()).unwrap();
    let mut sizes: Vec<u32> = icon_dir.entries().iter().map(|e| e.width()).collect();
    sizes.sort();
    assert_eq!(sizes, vec![16, 32, 48]);
}

#[test]
fn apple_touch_icon_is_opaque_180() {
    let dir = build_tmp(build_web);
    let img = read_png_rgba(&dir.join("web/apple-touch-icon.png"));
    assert_eq!((img.width, img.height), (180, 180));
    assert!(img.pixels().all(|[_, _, _, a]| a == 255));
}

#[test]
fn mark_512_is_512_square() {
    let dir = build_tmp(build_web);
    let img = read_png_rgba(&dir.join("mark/mark-512.png"));
    assert_eq!((img.width, img.height), (512, 512));
}

#[test]
fn mobile_builds_are_deterministic() {
    let build = |f: fn(&Path, &Path) -> anyhow::Result<()>| build_tmp(f);
    let ios_a = build(build_ios);
    let ios_b = build(build_ios);
    assert_eq!(tree(&ios_a), tree(&ios_b));
    let android_a = build(build_android);
    let android_b = build(build_android);
    assert_eq!(tree(&android_a), tree(&android_b));
    let web_a = build(build_web);
    let web_b = build(build_web);
    assert_eq!(tree(&web_a), tree(&web_b));
}
