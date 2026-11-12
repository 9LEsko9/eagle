// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared process entry point.

pub mod music;
mod playback;
pub mod session;

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let logger =
        eagle_platform::logging::Logger::init(&eagle_platform::logging::default_directory()?)?;
    log::info!(target: "eagle_app", "application started");
    logger.flush()?;
    if std::env::args().any(|arg| arg == "--bench-idle") {
        loop {
            std::thread::park();
        }
    }
    if std::env::args().any(|arg| arg == "--bench-info") {
        loop {
            log::info!(target: "eagle_app", provider = "fixture", track_id = 42; "benchmark event");
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
    let root = eagle_platform::logging::default_directory()?
        .parent()
        .ok_or("missing data directory")?
        .to_owned();
    if std::env::args().any(|arg| arg == "--bench-store") {
        return music::bench_store(&root);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    music::run(&args, &root)?;
    logger.flush()?;
    Ok(())
}

/// Shared application data location for CLI and desktop.
pub fn data_directory() -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    Ok(eagle_platform::logging::default_directory()?
        .parent()
        .ok_or("missing data directory")?
        .to_owned())
}
