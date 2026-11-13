// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use eagle_brand::palette::{self, DISC, GOLD, GRAPHITE};
use eagle_brand::render::{recolor_background, render_svg, tint, to_template};

const TEST_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><rect id="bg" width="10" height="10" fill="#1A1A1D"/><circle cx="5" cy="5" r="2" fill="#E9A23B"/></svg>"##;

#[test]
fn render_keeps_exact_bg_color() {
    let p = render_svg(TEST_SVG, 64).unwrap();
    assert_eq!((p.width(), p.height()), (64, 64));
    let c = p.pixel(1, 1).unwrap();
    assert_eq!(
        [c.red(), c.green(), c.blue(), c.alpha()],
        [0x1A, 0x1A, 0x1D, 255]
    );
}

#[test]
fn recolor_changes_bg_only() {
    let s = recolor_background(TEST_SVG, "#3E2430");
    let p = render_svg(&s, 64).unwrap();
    let c = p.pixel(1, 1).unwrap();
    assert_eq!([c.red(), c.green(), c.blue()], [0x3E, 0x24, 0x30]);
    let m = p.pixel(32, 32).unwrap();
    assert_eq!([m.red(), m.green(), m.blue()], [0xE9, 0xA2, 0x3B]);
}

#[test]
fn template_is_black_with_alpha() {
    let p = to_template(&render_svg(TEST_SVG, 32).unwrap());
    assert!(
        p.pixels()
            .iter()
            .all(|c| c.red() == 0 && c.green() == 0 && c.blue() == 0)
    );
}

#[test]
fn tint_sets_rgb_and_keeps_alpha() {
    let src = render_svg(TEST_SVG, 32).unwrap();
    let p = tint(&src, [0x12, 0x34, 0x56]);
    let corner = p.pixel(1, 1).unwrap();
    assert_eq!(
        [corner.red(), corner.green(), corner.blue(), corner.alpha()],
        [0x12, 0x34, 0x56, 255]
    );
    assert!(
        p.pixels()
            .iter()
            .zip(src.pixels())
            .all(|(a, b)| a.alpha() == b.alpha())
    );
}

#[test]
fn palette_constants_and_rgb_parse() {
    assert_eq!(GRAPHITE, "#1A1A1D");
    assert_eq!(DISC, "#F2F2F4");
    assert_eq!(GOLD, "#E9A23B");
    assert_eq!(palette::rgb(GOLD), [0xE9, 0xA2, 0x3B]);
    assert_eq!(palette::rgb("#000000"), [0, 0, 0]);
}

#[test]
fn rgb_returns_black_for_malformed_hex() {
    for bad in [
        "#1AZZ1D", "#+F0000", "1A1A1D", "#1A1A", "", "#1A1A1D0", "#ÄÄÄ",
    ] {
        assert_eq!(palette::rgb(bad), [0, 0, 0], "input {bad:?}");
    }
}
