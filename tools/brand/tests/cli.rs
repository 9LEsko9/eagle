// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

mod common;

use std::fs;
use std::path::Path;
use std::process::Command;

use common::{SRC, hash_tree, read_png_rgba};
use eagle_brand::build::build_all;
use tempfile::TempDir;

#[test]
fn build_is_deterministic() {
    let a = TempDir::new().unwrap();
    let b = TempDir::new().unwrap();
    build_all(Path::new(SRC), a.path()).unwrap();
    build_all(Path::new(SRC), b.path()).unwrap();
    assert_eq!(hash_tree(a.path()), hash_tree(b.path()));
}

#[test]
fn build_cleans_stale_files() {
    let d = TempDir::new().unwrap();
    fs::write(d.path().join("stale.txt"), "x").unwrap();
    build_all(Path::new(SRC), d.path()).unwrap();
    assert!(!d.path().join("stale.txt").exists());
    assert!(d.path().join("preview.png").exists());
}

#[test]
fn cli_build_writes_all_outputs_and_preview() {
    let d = TempDir::new().unwrap();
    let out = d.path().join("out");
    let status = Command::new(env!("CARGO_BIN_EXE_eagle-brand"))
        .args(["build", "--src", SRC, "--out"])
        .arg(&out)
        .status()
        .unwrap();
    assert!(status.success());
    assert!(out.join("desktop/eagle.ico").exists());
    assert!(out.join("tray/macos/trayTemplate.png").exists());
    assert!(out.join("ios/AppIcon.appiconset/Contents.json").exists());
    assert!(
        out.join("android/res/mipmap-anydpi-v26/ic_launcher.xml")
            .exists()
    );
    assert!(out.join("web/favicon.ico").exists());
    let preview = read_png_rgba(&out.join("preview.png"));
    assert_eq!((preview.width, preview.height), (1600, 1000));
}
