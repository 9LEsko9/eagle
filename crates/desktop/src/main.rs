// SPDX-License-Identifier: GPL-3.0-or-later
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let logger =
        eagle_platform::logging::Logger::init(&eagle_platform::logging::default_directory()?)?;
    let result = eagle_ui::run(
        eagle_app::data_directory()?,
        std::env::args().any(|arg| arg == "--software"),
    );
    logger.flush()?;
    result
}

#[cfg(any(target_os = "android", target_os = "ios"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    eagle_app::run()
}
