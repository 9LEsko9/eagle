// SPDX-License-Identifier: GPL-3.0-or-later
use crate::{DesktopWindow, Motion, Theme, ThemeMode, Type, Ui};
use serde::{Deserialize, Serialize};
use slint::ComponentHandle;
use std::path::Path;
#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub width: u32,
    pub height: u32,
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub theme: u8,
    pub panel_open: bool,
    pub reduced: bool,
    pub text_scale: f32,
    pub tint: u8,
    pub list_covers: bool,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 880,
            x: None,
            y: None,
            theme: 0,
            panel_open: false,
            reduced: false,
            text_scale: 1.,
            tint: 1,
            list_covers: true,
        }
    }
}
impl Preferences {
    pub fn load(root: &Path) -> Self {
        std::fs::read(root.join("desktop.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }
    pub fn apply(&self, ui: &DesktopWindow) {
        if (800..=7680).contains(&self.width) && (480..=4320).contains(&self.height) {
            ui.window().set_size(slint::LogicalSize::new(
                self.width as f32,
                self.height as f32,
            ));
        }
        ui.global::<Theme>().set_mode(match self.theme {
            1 => ThemeMode::Graphite,
            2 => ThemeMode::Paper,
            3 => ThemeMode::Oled,
            _ => ThemeMode::Auto,
        });
        ui.global::<Theme>().set_tint(i32::from(self.tint.min(2)));
        ui.global::<Ui>().set_panel_open(self.panel_open);
        ui.global::<Ui>().set_list_covers(self.list_covers);
        ui.global::<Motion>().set_reduced(self.reduced);
        ui.global::<Type>()
            .set_scale(if (0.8..=1.8).contains(&self.text_scale) {
                self.text_scale
            } else {
                1.
            });
    }
    pub fn capture(ui: &DesktopWindow) -> Self {
        let size = ui.window().size().to_logical(ui.window().scale_factor());
        let pos = ui.window().position();
        Self {
            width: size.width.round() as u32,
            height: size.height.round() as u32,
            x: Some(pos.x),
            y: Some(pos.y),
            theme: match ui.global::<Theme>().get_mode() {
                ThemeMode::Auto => 0,
                ThemeMode::Graphite => 1,
                ThemeMode::Paper => 2,
                ThemeMode::Oled => 3,
            },
            panel_open: ui.global::<Ui>().get_panel_open(),
            reduced: ui.global::<Motion>().get_reduced(),
            text_scale: ui.global::<Type>().get_scale(),
            tint: ui.global::<Theme>().get_tint() as u8,
            list_covers: ui.global::<Ui>().get_list_covers(),
        }
    }
    pub fn save(&self, root: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let temporary = root.join("desktop.json.tmp");
        std::fs::write(&temporary, serde_json::to_vec(self)?)?;
        std::fs::rename(temporary, root.join("desktop.json"))?;
        Ok(())
    }
}
