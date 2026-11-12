// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn logging_child() {
    let Some(directory) = std::env::var_os("EAGLE_LOG_CHILD") else {
        return;
    };
    let logger = eagle_platform::logging::Logger::init(std::path::Path::new(&directory)).unwrap();
    log::info!(token = "LEAKME"; "buffered before warning");
    log::warn!("sign=LEAKME");
    let durable = fs::read_to_string(std::path::Path::new(&directory).join("eagle.log")).unwrap();
    assert!(durable.contains("buffered before warning"));
    assert!(durable.contains("sign=[REDACTED]"));
    logger.set_filter("off,eagle_platform=debug").unwrap();
    log::info!(target: "other", "must stay filtered");
    log::debug!(target: "eagle_platform", "settings enabled debug");
    assert!(logger.report().unwrap().contains("settings enabled debug"));
    log::info!(target: "eagle_platform", "buffered before panic");
    panic!("access_token=LEAKME");
}

#[test]
fn installed_hook_and_sinks_preserve_privacy() {
    let directory = std::env::temp_dir().join(format!(
        "eagle-log-child-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "logging_child", "--nocapture"])
        .env("EAGLE_LOG_CHILD", &directory)
        .env("EAGLE_LOG", "info")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let report = fs::read_to_string(directory.join("eagle.log")).unwrap();
    assert!(report.contains("buffered before panic"), "{report}");
    assert!(report.contains("PANIC"));
    assert!(!report.contains("must stay filtered"));
    assert!(!report.contains("LEAKME"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("LEAKME"));
    fs::remove_dir_all(directory).unwrap();
}
