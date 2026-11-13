// SPDX-License-Identifier: GPL-3.0-or-later
//! Per-user desktop autostart registration.
use std::io;

#[cfg(not(target_os = "windows"))]
use std::{
    fs,
    path::{Path, PathBuf},
};

#[cfg(target_os = "macos")]
fn registration() -> io::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Library/LaunchAgents/org.eagle.desktop.plist"))
        .ok_or_else(|| io::Error::other("HOME is unavailable"))
}
#[cfg(all(unix, not(target_os = "macos")))]
fn registration() -> io::Result<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .map(|base| base.join("autostart/eagle.desktop"))
        .ok_or_else(|| io::Error::other("configuration directory is unavailable"))
}

#[cfg(not(target_os = "windows"))]
pub fn enabled() -> io::Result<bool> {
    match fs::metadata(registration()?) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(not(target_os = "windows"))]
pub fn set_enabled(enable: bool) -> io::Result<()> {
    let path = registration()?;
    if enable {
        let exe = std::env::current_exe()?;
        set_file(&path, Some(&exe))
    } else {
        set_file(&path, None)
    }
}

#[cfg(not(target_os = "windows"))]
fn set_file(path: &Path, exe: Option<&Path>) -> io::Result<()> {
    let Some(exe) = exe else {
        return match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        };
    };
    let content = registration_content(exe)?;
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("missing registration directory"))?;
    fs::create_dir_all(dir)?;
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, content)?;
    fs::rename(temporary, path)
}

#[cfg(target_os = "macos")]
fn registration_content(exe: &Path) -> io::Result<String> {
    let value = exe.to_str().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "executable path is not UTF-8")
    })?;
    let value = value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>Label</key><string>org.eagle.desktop</string><key>ProgramArguments</key><array><string>{value}</string></array><key>RunAtLoad</key><true/></dict></plist>\n"
    ))
}
#[cfg(all(unix, not(target_os = "macos")))]
fn registration_content(exe: &Path) -> io::Result<String> {
    let value = exe.to_str().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "executable path is not UTF-8")
    })?;
    if value.contains(['\n', '\r']) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid executable path",
        ));
    }
    let value = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('`', "\\`")
        .replace('$', "\\$")
        .replace('%', "%%");
    Ok(format!(
        "[Desktop Entry]\nType=Application\nName=eagle\nExec=\"{value}\"\n"
    ))
}

#[cfg(target_os = "windows")]
const KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
#[cfg(target_os = "windows")]
pub fn enabled() -> io::Result<bool> {
    let output = std::process::Command::new("reg.exe")
        .args(["query", KEY, "/v", "eagle"])
        .output()?;
    Ok(output.status.success())
}
#[cfg(target_os = "windows")]
pub fn set_enabled(enable: bool) -> io::Result<()> {
    let status = if enable {
        let exe = std::env::current_exe()?;
        let value = format!("\"{}\"", exe.display());
        std::process::Command::new("reg.exe")
            .args([
                "add", KEY, "/v", "eagle", "/t", "REG_SZ", "/d", &value, "/f",
            ])
            .status()?
    } else {
        if !enabled()? {
            return Ok(());
        }
        std::process::Command::new("reg.exe")
            .args(["delete", KEY, "/v", "eagle", "/f"])
            .status()?
    };
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other("autostart registration failed"))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn registration_can_be_enabled_and_disabled_in_isolated_directory() {
        let dir = std::env::temp_dir().join(format!("eagle-autostart-test-{}", std::process::id()));
        let file = dir.join("autostart");
        set_file(&file, Some(Path::new("/tmp/eagle"))).unwrap();
        assert!(fs::read_to_string(&file).unwrap().contains("/tmp/eagle"));
        set_file(&file, None).unwrap();
        set_file(&file, None).unwrap();
        assert!(!file.exists());
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn executable_path_is_escaped_in_registration() {
        let content = registration_content(Path::new("/tmp/eagle & music/$test%")).unwrap();
        #[cfg(target_os = "macos")]
        assert!(content.contains("/tmp/eagle &amp; music/$test%"));
        #[cfg(not(target_os = "macos"))]
        assert!(content.contains("/tmp/eagle & music/\\$test%%"));
    }
}
