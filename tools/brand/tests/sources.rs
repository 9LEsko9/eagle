// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use eagle_brand::render::render_svg;
use eagle_brand::sources::{ALL_SOURCES, Source, for_size, load};
use resvg::usvg;

const SRC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/brand/src");

#[test]
fn icon_has_required_ids() {
    let t = usvg::Tree::from_str(&load(SRC, Source::Icon).unwrap(), &Default::default()).unwrap();
    for id in [
        "bg", "disc", "groove", "head", "beak", "eye", "cere", "mouth",
    ] {
        assert!(t.node_by_id(id).is_some(), "missing #{id}");
    }
}

#[test]
fn gold_only_on_beak() {
    for s in ALL_SOURCES {
        let svg = load(SRC, s).unwrap();
        let n = svg.matches("#E9A23B").count();
        let expected = if matches!(
            s,
            Source::Icon | Source::IconSmall | Source::Mark | Source::AndroidFg
        ) {
            1
        } else {
            0
        };
        assert_eq!(n, expected, "{s:?}");
    }
}

#[test]
fn tray_small_keeps_cutout_at_16() {
    let p = render_svg(&load(SRC, Source::TraySmall).unwrap(), 16).unwrap();
    let inside_disc_transparent = (4..12)
        .flat_map(|y| (4..12).map(move |x| (x, y)))
        .filter(|&(x, y)| p.pixel(x, y).unwrap().alpha() < 128)
        .count();
    assert!(
        inside_disc_transparent >= 6,
        "head cutout lost at 16px ({inside_disc_transparent} transparent pixels)"
    );
}

#[test]
fn android_fg_inside_safe_zone() {
    let p = render_svg(&load(SRC, Source::AndroidFg).unwrap(), 432).unwrap();
    let r_max = 432.0 * 0.3055;
    for y in 0..432 {
        for x in 0..432 {
            let (dx, dy) = (x as f32 + 0.5 - 216.0, y as f32 + 0.5 - 216.0);
            if (dx * dx + dy * dy).sqrt() > r_max + 1.0 {
                assert_eq!(
                    p.pixel(x, y).unwrap().alpha(),
                    0,
                    "pixel {x},{y} outside safe zone"
                );
            }
        }
    }
}

#[test]
fn tray_and_mono_are_single_colour() {
    for s in [Source::Tray, Source::TraySmall, Source::AndroidMono] {
        let svg = load(SRC, s).unwrap();
        assert!(
            !svg.contains("#1A1A1D") && !svg.contains("#F2F2F4") && !svg.contains("#E9A23B"),
            "{s:?}"
        );
    }
}

#[test]
fn for_size_picks_small() {
    assert_eq!(for_size(Source::Icon, 32), Source::IconSmall);
    assert_eq!(for_size(Source::Icon, 33), Source::Icon);
    assert_eq!(for_size(Source::Tray, 18), Source::TraySmall);
    assert_eq!(for_size(Source::Mark, 16), Source::Mark);
}
