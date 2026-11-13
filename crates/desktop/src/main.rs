// SPDX-License-Identifier: GPL-3.0-or-later
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bootstrap = eagle_app::desktop_bootstrap()?;
    // Logging stays outside the directory that Session can relocate.
    let logger = eagle_platform::logging::Logger::init(&bootstrap.join("logs"))?;
    let args: Vec<String> = std::env::args().collect();
    if let Some(index) = args.iter().position(|arg| arg == "--fixture-root") {
        let root = std::path::PathBuf::from(
            args.get(index + 1)
                .ok_or("--fixture-root requires a path")?,
        );
        if root.exists() && !root.join(".eagle-fixture").exists() {
            return Err("fixture root must be new or marked as an eagle fixture".into());
        }
        std::fs::create_dir_all(&root)?;
        std::fs::write(root.join(".eagle-fixture"), b"fixture")?;
        eagle_app::fixture::seed(&root)?;
        return eagle_ui::run_with_providers(
            root,
            args.iter().any(|arg| arg == "--software"),
            vec![eagle_app::fixture::provider()],
        );
    }
    let result = eagle_ui::run_with_options(
        eagle_app::data_directory()?,
        bootstrap,
        std::env::args().any(|arg| arg == "--software"),
    );
    logger.flush()?;
    result
}

#[cfg(any(target_os = "android", target_os = "ios"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    eagle_app::run()
}
