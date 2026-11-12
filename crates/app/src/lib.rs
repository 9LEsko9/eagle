// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared process entry point.

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
    logger.flush()?;
    Ok(())
}
