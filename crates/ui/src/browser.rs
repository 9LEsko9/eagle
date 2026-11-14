// SPDX-License-Identifier: GPL-3.0-or-later
//! Opens a link in the system browser with the platform's own tool, so no extra dependency.
use std::process::{Command, Stdio};

/// Plain `https` links only: nothing a shell or a handler could take for an option or a command.
fn allowed(url: &str) -> bool {
    url.starts_with("https://")
        && url.len() <= 2048
        && url.bytes().all(|b| {
            b.is_ascii_graphic()
                && !matches!(b, b'"' | b'\'' | b'<' | b'>' | b'^' | b'|' | b'`' | b'\\')
        })
}

/// Starts the browser; false when the link is refused or the tool is missing.
pub(crate) fn open(url: &str) -> bool {
    if !allowed(url) {
        log::warn!("sign-in link refused");
        return false;
    }
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");
    #[cfg(target_os = "windows")]
    let mut command = Command::new("rundll32");
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut command = Command::new("xdg-open");
    #[cfg(target_os = "windows")]
    command.arg("url.dll,FileProtocolHandler");
    command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match command.spawn() {
        Ok(mut child) => {
            // Reaps the launcher; the browser itself outlives it.
            let _ = std::thread::Builder::new()
                .name("eg-open-url".into())
                .spawn(move || {
                    let _ = child.wait();
                });
            true
        }
        Err(error) => {
            log::warn!(error = error.to_string().as_str(); "could not start the browser");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::allowed;

    #[test]
    fn only_plain_https_links_are_opened() {
        assert!(allowed("https://ya.ru/device"));
        assert!(allowed("https://id.example/login?a=1&b=2"));
        for url in [
            "http://ya.ru/device",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "https://x y",
            "https://x\"; calc",
            "--help",
            "",
        ] {
            assert!(!allowed(url), "{url}");
        }
    }
}
