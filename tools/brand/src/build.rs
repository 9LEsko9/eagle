// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Writes the desktop, tray, mobile and web assets.

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use resvg::tiny_skia::{Color, Pixmap, PixmapPaint, Transform};

use crate::pack::{write_icns, write_ico, write_png};
use crate::palette::{GRAPHITE, rgb};
use crate::preview;
use crate::render::{render_svg, tint, to_template};
use crate::sources::{Source, for_size, load};

/// Sizes of the Windows executable icon.
const ICO_SIZES: [u32; 8] = [16, 20, 24, 32, 40, 48, 64, 256];
/// Sizes of the macOS icon and of the PNG icons.
const ICNS_SIZES: [u32; 7] = [16, 32, 64, 128, 256, 512, 1024];
/// Sizes of the Windows tray glyphs.
const WIN_TRAY_SIZES: [u32; 4] = [16, 20, 24, 32];
/// Sizes of the Linux tray PNGs.
const LINUX_TRAY_SIZES: [u32; 2] = [22, 24];
/// Color of the light tray glyph on dark panels.
const WHITE: [u8; 3] = [255, 255, 255];
/// Android density buckets: name, adaptive layer size and legacy icon size.
const ANDROID_DENSITIES: [(&str, u32, u32); 5] = [
    ("mdpi", 108, 48),
    ("hdpi", 162, 72),
    ("xhdpi", 216, 96),
    ("xxhdpi", 324, 144),
    ("xxxhdpi", 432, 192),
];
/// Adaptive icon layers: file name suffix and source.
const ANDROID_LAYERS: [(&str, Source); 3] = [
    ("foreground", Source::AndroidFg),
    ("background", Source::AndroidBg),
    ("monochrome", Source::AndroidMono),
];
/// Adaptive icon description that refers to the layer PNGs.
const ADAPTIVE_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<adaptive-icon xmlns:android="http://schemas.android.com/apk/res/android">
    <background android:drawable="@mipmap/ic_launcher_background"/>
    <foreground android:drawable="@mipmap/ic_launcher_foreground"/>
    <monochrome android:drawable="@mipmap/ic_launcher_monochrome"/>
</adaptive-icon>
"#;
/// Xcode asset catalog entry for the single 1024 px app icon.
const CONTENTS_JSON: &str = r#"{
  "images": [
    {
      "idiom": "universal",
      "platform": "ios",
      "size": "1024x1024",
      "filename": "AppIcon-1024.png"
    }
  ],
  "info": {
    "author": "eagle-brand",
    "version": 1
  }
}
"#;
/// Sizes of the favicon ICO.
const FAVICON_SIZES: [u32; 3] = [16, 32, 48];
/// Size of the Apple touch icon.
const APPLE_TOUCH_SIZE: u32 = 180;
/// Size of the mark PNG.
const MARK_SIZE: u32 = 512;

/// Renders `large` at `size` pixels, using the small variant where one exists.
fn render_at(src: &Path, large: Source, size: u32) -> Result<Pixmap> {
    let svg = load(src, for_size(large, size))?;
    render_svg(&svg, size)
}

/// Renders `large` at each of `sizes`.
fn render_all(src: &Path, large: Source, sizes: &[u32]) -> Result<Vec<Pixmap>> {
    sizes.iter().map(|&s| render_at(src, large, s)).collect()
}

/// Creates `dir` and its parents.
fn mkdir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))
}

/// Removes `out`, then writes every asset and the preview sheet into it.
///
/// Refuses to run when `out` is a dangerous target (see `check_out`).
pub fn build_all(src: &Path, out: &Path) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read the current directory")?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    check_out(src, out, &cwd, home.as_deref())?;
    if fs::symlink_metadata(out).is_ok() {
        fs::remove_dir_all(out).with_context(|| format!("failed to clear {}", out.display()))?;
    }
    mkdir(out)?;
    build_desktop(src, out)?;
    build_tray(src, out)?;
    build_ios(src, out)?;
    build_android(src, out)?;
    build_web(src, out)?;
    preview::write_sheet(src, out)
}

/// Rejects an `out` directory that `build_all` must never clear.
///
/// `out` may not contain `.` or `..`; `src` is only read, so it may. Both are
/// resolved against `cwd` with symlinks followed, so the checks apply to the directory that
/// removal would really touch. Refused: an empty path, the filesystem root,
/// a directory that contains `src`, `cwd` or `home`, or `cwd` itself.
fn check_out(src: &Path, out: &Path, cwd: &Path, home: Option<&Path>) -> Result<()> {
    if out.as_os_str().is_empty() {
        bail!("refusing to clear an empty output path");
    }
    if out
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        bail!(
            "refusing {}: use a plain path without '.' or '..'",
            out.display()
        );
    }
    let cwd = resolve(cwd)?;
    let src = resolve(&cwd.join(src))?;
    let out = resolve(&cwd.join(out))?;
    if out.parent().is_none() {
        bail!("refusing to clear the filesystem root {}", out.display());
    }
    if src.starts_with(&out) {
        bail!(
            "refusing to clear {}, which contains the source",
            out.display()
        );
    }
    if cwd.starts_with(&out) {
        bail!(
            "refusing to clear {}, an ancestor of the working directory",
            out.display()
        );
    }
    if let Some(home) = home
        && resolve(home)?.starts_with(&out)
    {
        bail!(
            "refusing to clear {}, an ancestor of the home directory",
            out.display()
        );
    }
    Ok(())
}

/// Resolves the absolute `path` to a real path, following symlinks.
///
/// The longest existing prefix is canonicalized and the missing rest is
/// appended, so the result is the path a later `create_dir_all` would use.
fn resolve(path: &Path) -> Result<PathBuf> {
    let mut missing = Vec::new();
    let mut current = path;
    loop {
        if let Ok(real) = fs::canonicalize(current) {
            return Ok(missing.iter().rev().fold(real, |acc, part| acc.join(part)));
        }
        let (Some(parent), Some(name)) = (current.parent(), current.file_name()) else {
            bail!("cannot resolve {}", path.display());
        };
        missing.push(name.to_owned());
        current = parent;
    }
}

/// Writes the desktop icons into `out`.
///
/// Produces `desktop/eagle.ico`, `desktop/eagle.icns` and `png/icon-{size}.png`.
pub fn build_desktop(src: &Path, out: &Path) -> Result<()> {
    let desktop = out.join("desktop");
    let png_dir = out.join("png");
    mkdir(&desktop)?;
    mkdir(&png_dir)?;

    let sized = render_all(src, Source::Icon, &ICNS_SIZES)?;
    write_icns(&sized, &desktop.join("eagle.icns"))?;
    for p in &sized {
        write_png(p, &png_dir.join(format!("icon-{}.png", p.width())))?;
    }

    let ico_images = render_all(src, Source::Icon, &ICO_SIZES)?;
    write_ico(&ico_images, &desktop.join("eagle.ico"))
}

/// Writes the tray icons into `out`.
///
/// macOS gets black template PNGs. Windows gets a black and a white ICO.
/// Linux gets a symbolic SVG and white PNGs.
pub fn build_tray(src: &Path, out: &Path) -> Result<()> {
    let macos = out.join("tray/macos");
    mkdir(&macos)?;
    for (name, size) in [("trayTemplate.png", 18), ("trayTemplate@2x.png", 36)] {
        let p = render_at(src, Source::Tray, size)?;
        write_png(&to_template(&p), &macos.join(name))?;
    }

    let windows = out.join("tray/windows");
    mkdir(&windows)?;
    let glyphs = render_all(src, Source::Tray, &WIN_TRAY_SIZES)?;
    let light: Vec<Pixmap> = glyphs.iter().map(to_template).collect();
    write_ico(&light, &windows.join("tray-light.ico"))?;
    let dark: Vec<Pixmap> = glyphs.iter().map(|p| tint(p, WHITE)).collect();
    write_ico(&dark, &windows.join("tray-dark.ico"))?;

    let linux = out.join("tray/linux");
    mkdir(&linux)?;
    fs::copy(
        src.join(Source::Tray.file_name()),
        linux.join("tray-symbolic.svg"),
    )
    .context("failed to copy tray-symbolic.svg")?;
    for size in LINUX_TRAY_SIZES {
        let p = tint(&render_at(src, Source::Tray, size)?, WHITE);
        write_png(&p, &linux.join(format!("tray-{size}.png")))?;
    }
    Ok(())
}

/// Writes `text` to `path`, replacing any existing file.
fn write_text(path: &Path, text: &str) -> Result<()> {
    fs::write(path, text).with_context(|| format!("failed to write {}", path.display()))
}

/// Renders the full icon at `size` on an opaque background.
///
/// The icon has an opaque background, so the graphite fill is only used if
/// the render still leaves transparent pixels.
fn opaque_icon(src: &Path, size: u32) -> Result<Pixmap> {
    let icon = render_at(src, Source::Icon, size)?;
    if icon.pixels().iter().all(|c| c.alpha() == u8::MAX) {
        return Ok(icon);
    }
    let mut out =
        Pixmap::new(icon.width(), icon.height()).context("pixmap size must be non-zero")?;
    let [r, g, b] = rgb(GRAPHITE);
    out.fill(Color::from_rgba8(r, g, b, u8::MAX));
    out.draw_pixmap(
        0,
        0,
        icon.as_ref(),
        &PixmapPaint::default(),
        Transform::identity(),
        None,
    );
    Ok(out)
}

/// Writes the iOS app icon set into `out`.
///
/// Produces `ios/AppIcon.appiconset/AppIcon-1024.png` and `Contents.json`.
pub fn build_ios(src: &Path, out: &Path) -> Result<()> {
    let set = out.join("ios/AppIcon.appiconset");
    mkdir(&set)?;
    write_png(&opaque_icon(src, 1024)?, &set.join("AppIcon-1024.png"))?;
    write_text(&set.join("Contents.json"), CONTENTS_JSON)
}

/// Writes the Android launcher icons into `out`.
///
/// Produces the adaptive layers and legacy icon in `android/res/mipmap-*`
/// and the adaptive icon description in `mipmap-anydpi-v26/ic_launcher.xml`.
pub fn build_android(src: &Path, out: &Path) -> Result<()> {
    let res = out.join("android/res");
    let adaptive = res.join("mipmap-anydpi-v26");
    mkdir(&adaptive)?;
    for (density, layer_size, legacy_size) in ANDROID_DENSITIES {
        let dir = res.join(format!("mipmap-{density}"));
        mkdir(&dir)?;
        for (layer, source) in ANDROID_LAYERS {
            let p = render_at(src, source, layer_size)?;
            write_png(&p, &dir.join(format!("ic_launcher_{layer}.png")))?;
        }
        let legacy = render_at(src, Source::Icon, legacy_size)?;
        write_png(&legacy, &dir.join("ic_launcher.png"))?;
    }
    write_text(&adaptive.join("ic_launcher.xml"), ADAPTIVE_XML)
}

/// Writes the web icons into `out`.
///
/// Produces `web/favicon.svg`, `web/favicon.ico`, `web/apple-touch-icon.png`
/// and `mark/mark-512.png`.
pub fn build_web(src: &Path, out: &Path) -> Result<()> {
    let web = out.join("web");
    let mark = out.join("mark");
    mkdir(&web)?;
    mkdir(&mark)?;

    fs::copy(src.join(Source::Mark.file_name()), web.join("favicon.svg"))
        .context("failed to copy favicon.svg")?;
    let favicon = render_all(src, Source::Icon, &FAVICON_SIZES)?;
    write_ico(&favicon, &web.join("favicon.ico"))?;
    write_png(
        &opaque_icon(src, APPLE_TOUCH_SIZE)?,
        &web.join("apple-touch-icon.png"),
    )?;
    write_png(
        &render_at(src, Source::Mark, MARK_SIZE)?,
        &mark.join("mark-512.png"),
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::check_out;
    use tempfile::TempDir;

    /// A real directory layout: `base/work/src` is the source, `work` is the cwd.
    struct Layout {
        _dir: TempDir,
        base: PathBuf,
        work: PathBuf,
        src: PathBuf,
    }

    fn layout() -> Layout {
        let dir = TempDir::new().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let work = base.join("work");
        let src = work.join("src");
        fs::create_dir_all(&src).unwrap();
        Layout {
            _dir: dir,
            base,
            work,
            src,
        }
    }

    #[test]
    fn check_out_refuses_dangerous_targets() {
        let l = layout();
        let (src, cwd) = (l.src.as_path(), l.work.as_path());
        let refused = [
            "".to_string(),
            "/".to_string(),
            ".".to_string(),
            "..".to_string(),
            "src/..".to_string(),
            "../../..".to_string(),
            src.display().to_string(),
            l.work.display().to_string(),
            l.base.display().to_string(),
        ];
        for out in &refused {
            assert!(
                check_out(src, Path::new(out), cwd, None).is_err(),
                "{out:?} must be refused"
            );
        }
        let allowed = [l.work.join("out"), l.src.join("out"), PathBuf::from("out")];
        for out in &allowed {
            assert!(
                check_out(src, out, cwd, None).is_ok(),
                "{out:?} must be allowed"
            );
        }
        let home = l.base.join("home");
        assert!(check_out(src, &l.base.join("home/out"), cwd, Some(&home)).is_ok());
        assert!(
            check_out(src, &l.base, cwd, Some(&home)).is_err(),
            "ancestor of home"
        );
    }

    #[cfg(unix)]
    #[test]
    fn check_out_follows_symlinks() {
        let l = layout();
        let alias = l.base.join("alias");
        std::os::unix::fs::symlink(&l.work, &alias).unwrap();
        let (src, cwd) = (l.src.as_path(), l.work.as_path());
        assert!(
            check_out(src, &alias.join("src"), cwd, None).is_err(),
            "resolves to src"
        );
        assert!(
            check_out(src, &alias, cwd, None).is_err(),
            "resolves to cwd"
        );
        assert!(check_out(src, &alias.join("out"), cwd, None).is_ok());
    }
}
