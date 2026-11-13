// SPDX-License-Identifier: GPL-3.0-or-later
//! First-close notification; failure leaves the marker unset for a later attempt.
use std::{path::PathBuf, process::Command};

pub fn first_close(root: PathBuf) {
    if root.join("background-notified").exists() {
        return;
    }
    let _ = std::thread::Builder::new().name("eg-notification".into()).spawn(move || {
        #[cfg(target_os = "macos")]
        let result = Command::new("osascript").args(["-e", "display notification \"Eagle продолжает играть в фоне. Чтобы открыть окно, запустите приложение снова.\" with title \"Eagle\""]).status();
        #[cfg(target_os = "linux")]
        let result = Command::new("notify-send").args(["Eagle", "Eagle продолжает играть в фоне. Чтобы открыть окно, запустите приложение снова."]).status();
        #[cfg(target_os = "windows")]
        let result = Command::new("powershell.exe").args(["-NoProfile", "-NonInteractive", "-Command", "$ErrorActionPreference='Stop'; [Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType=WindowsRuntime] > $null; $xml = New-Object Windows.Data.Xml.Dom.XmlDocument; $xml.LoadXml('<toast><visual><binding template=\"ToastGeneric\"><text>Eagle</text><text>Eagle продолжает играть в фоне. Запустите приложение снова, чтобы открыть окно.</text></binding></visual></toast>'); [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('Eagle').Show([Windows.UI.Notifications.ToastNotification]::new($xml))"]).status();
        if result.is_ok_and(|status| status.success()) {
            let _ = std::fs::write(root.join("background-notified"), b"1");
        }
    });
}
