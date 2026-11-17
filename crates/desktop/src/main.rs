// SPDX-License-Identifier: GPL-3.0-or-later
#[cfg(target_os = "macos")]
mod macos_allocator;

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "macos")]
    macos_allocator::initialize()?;
    let bootstrap = eagle_app::desktop_bootstrap()?;
    // Logging stays outside the directory that Session can relocate.
    let logger = eagle_platform::logging::Logger::init(&bootstrap.join("logs"))?;
    let args: Vec<String> = std::env::args().collect();
    let renderer = eagle_ui::Renderer::from_args(&args)?;
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
        if !root.join(".fixture-prepared").exists() {
            eagle_app::fixture::seed(&root)?;
        }
        if args.iter().any(|arg| arg == "--fixture-prepare-only") {
            return Ok(());
        }
        #[cfg(target_os = "macos")]
        std::fs::write(
            root.join("fixture-allocator.env"),
            macos_allocator::configuration(),
        )?;
        let result = eagle_ui::run_with_providers(
            root.clone(),
            renderer,
            vec![eagle_app::fixture::stress_provider()],
        );
        logger.flush()?;
        #[cfg(target_os = "macos")]
        std::fs::write(root.join("fixture-peak.bytes"), fixture_peak()?.to_string())?;
        return result;
    }
    let result = eagle_ui::run_with_options(eagle_app::data_directory()?, bootstrap, renderer);
    logger.flush()?;
    result
}

#[cfg(target_os = "macos")]
fn fixture_peak() -> std::io::Result<u64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage_info_v4>::zeroed();
    // The flavor specifies the layout of the writable buffer; only read it on success.
    let status = unsafe {
        libc::proc_pid_rusage(
            libc::getpid(),
            libc::RUSAGE_INFO_V4,
            usage.as_mut_ptr().cast(),
        )
    };
    if status != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let usage = unsafe { usage.assume_init() };
    if usage.ri_lifetime_max_phys_footprint == 0
        || usage.ri_lifetime_max_phys_footprint < usage.ri_phys_footprint
    {
        return Err(std::io::Error::other("invalid lifetime physical footprint"));
    }
    Ok(usage.ri_lifetime_max_phys_footprint)
}

#[cfg(all(test, target_os = "macos"))]
#[test]
fn native_fixture_peak_is_positive_and_monotonic() {
    let first = fixture_peak().unwrap();
    assert!(first > 0);
    assert!(fixture_peak().unwrap() >= first);
}

#[cfg(any(target_os = "android", target_os = "ios"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    eagle_app::run()
}
