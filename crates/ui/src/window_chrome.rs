// SPDX-License-Identifier: GPL-3.0-or-later
//! Synchronize the native macOS title bar with the selected UI theme.
use crate::{DesktopWindow, Theme, ThemeMode};
use objc2::MainThreadMarker;
use objc2_app_kit::{NSColor, NSView};
use slint::ComponentHandle;
use slint::winit_030::{WinitWindowAccessor, winit};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

pub(super) fn apply(ui: &DesktopWindow) {
    if MainThreadMarker::new().is_none() {
        return;
    }
    let theme = ui.global::<Theme>();
    let background = theme.get_t().bg;
    let appearance = match theme.get_mode() {
        ThemeMode::Auto => None,
        ThemeMode::Paper => Some(winit::window::Theme::Light),
        ThemeMode::Graphite | ThemeMode::Oled => Some(winit::window::Theme::Dark),
    };
    ui.window().with_winit_window(|window| {
        window.set_theme(appearance);
        if appearance.is_none()
            && window.theme().is_some_and(|actual| {
                (actual == winit::window::Theme::Dark) != theme.get_system_dark()
            })
        {
            let weak = ui.as_weak();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = weak.upgrade() else {
                    return;
                };
                let theme = ui.global::<Theme>();
                if theme.get_mode() != ThemeMode::Auto {
                    return;
                }
                let actual = ui
                    .window()
                    .with_winit_window(|window| window.theme())
                    .flatten();
                if let Some(actual) = actual {
                    let dark = actual == winit::window::Theme::Dark;
                    if dark != theme.get_system_dark() {
                        theme.set_system_dark(dark);
                    }
                }
            });
        }
        let Ok(handle) = window.window_handle() else {
            return;
        };
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
            return;
        };
        // The handle borrows a live NSView from winit for the duration of this closure.
        let view = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
        let Some(native) = view.window() else {
            return;
        };
        let color = NSColor::colorWithSRGBRed_green_blue_alpha(
            f64::from(background.red()) / 255.0,
            f64::from(background.green()) / 255.0,
            f64::from(background.blue()) / 255.0,
            1.0,
        );
        native.setTitlebarAppearsTransparent(true);
        native.setBackgroundColor(Some(&color));
    });
}
