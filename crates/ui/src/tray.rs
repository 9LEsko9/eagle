// SPDX-License-Identifier: GPL-3.0-or-later
use eagle_app::session::{Command, CommandSender, PlayerState};
use eagle_engine::State;

fn action(sender: &CommandSender, id: &str) {
    match id {
        "open" => {
            let _ = slint::invoke_from_event_loop(|| {
                if let Err(error) = crate::show_window() {
                    log::warn!("open window: {error}");
                }
            });
        }
        "previous" => {
            let _ = sender.send(Command::Previous);
        }
        "toggle" => {
            let _ = sender.send(Command::TogglePlay);
        }
        "next" => {
            let _ = sender.send(Command::Next);
        }
        "quit" => {
            let _ = slint::quit_event_loop();
        }
        _ => {}
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
pub struct Tray {
    icon: tray_icon::TrayIcon,
    previous: tray_icon::menu::MenuItem,
    toggle: tray_icon::menu::MenuItem,
    next: tray_icon::menu::MenuItem,
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
impl Tray {
    pub fn new(sender: CommandSender) -> Result<Self, Box<dyn std::error::Error>> {
        use tray_icon::menu::{Menu, MenuEvent, MenuItem};
        let menu = Menu::new();
        let open = MenuItem::with_id("open", "Открыть", true, None);
        let previous = MenuItem::with_id("previous", "Предыдущий", false, None);
        let toggle = MenuItem::with_id("toggle", "Воспроизвести", false, None);
        let next = MenuItem::with_id("next", "Следующий", false, None);
        let quit = MenuItem::with_id("quit", "Выход", true, None);
        menu.append_items(&[&open, &previous, &toggle, &next, &quit])?;
        tray_icon::TrayIconEvent::set_event_handler(Some(|event| {
            if matches!(
                event,
                tray_icon::TrayIconEvent::Click {
                    button: tray_icon::MouseButton::Left,
                    button_state: tray_icon::MouseButtonState::Up,
                    ..
                }
            ) {
                let _ = slint::invoke_from_event_loop(|| {
                    if let Err(error) = crate::show_window() {
                        log::warn!("open window: {error}");
                    }
                });
            }
        }));
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            action(&sender, event.id.as_ref())
        }));
        let (pixels, width, height) = icon()?;
        let icon = tray_icon::Icon::from_rgba(pixels, width, height)?;
        let builder = tray_icon::TrayIconBuilder::new()
            .with_tooltip("eagle")
            .with_menu_on_left_click(false)
            .with_menu(Box::new(menu));
        #[cfg(target_os = "macos")]
        let builder = builder.with_icon_templated(icon);
        #[cfg(target_os = "windows")]
        let builder = builder.with_icon(icon);
        let tray = builder.build()?;
        Ok(Self {
            icon: tray,
            previous,
            toggle,
            next,
        })
    }
    pub fn update(&self, state: &PlayerState) {
        let available = state.track.is_some();
        self.previous.set_enabled(available);
        self.toggle.set_enabled(available);
        self.next.set_enabled(available);
        self.toggle.set_text(if state.status == State::Playing {
            "Пауза"
        } else {
            "Воспроизвести"
        });
        let title = state.track.as_ref().map_or_else(
            || "eagle".to_owned(),
            |track| format!("eagle — {}", track.title),
        );
        let _ = self.icon.set_tooltip(Some(title));
    }
}

#[cfg(target_os = "linux")]
struct LinuxTray {
    sender: CommandSender,
    title: String,
    playing: bool,
    available: bool,
}
#[cfg(target_os = "linux")]
impl ksni::Tray for LinuxTray {
    fn id(&self) -> String {
        "eagle".into()
    }
    fn title(&self) -> String {
        self.title.clone()
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let Ok((mut pixels, width, height)) = icon() else {
            return Vec::new();
        };
        for pixel in pixels.as_chunks_mut::<4>().0 {
            pixel.rotate_right(1);
        }
        vec![ksni::Icon {
            width: width as i32,
            height: height as i32,
            data: pixels,
        }]
    }
    fn activate(&mut self, _: i32, _: i32) {
        action(&self.sender, "open");
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        [
            ("open", "Открыть"),
            ("previous", "Предыдущий"),
            ("toggle", "Воспроизвести / пауза"),
            ("next", "Следующий"),
            ("quit", "Выход"),
        ]
        .into_iter()
        .map(|(id, label)| {
            ksni::menu::StandardItem {
                label: if id == "toggle" && self.playing {
                    "Пауза".into()
                } else {
                    label.into()
                },
                enabled: !matches!(id, "previous" | "toggle" | "next") || self.available,
                activate: Box::new(move |tray: &mut Self| action(&tray.sender, id)),
                ..Default::default()
            }
            .into()
        })
        .collect()
    }
}
#[cfg(target_os = "linux")]
pub struct Tray(ksni::blocking::Handle<LinuxTray>);
#[cfg(target_os = "linux")]
impl Tray {
    pub fn new(sender: CommandSender) -> Result<Self, Box<dyn std::error::Error>> {
        use ksni::blocking::TrayMethods;
        Ok(Self(
            LinuxTray {
                sender,
                title: "eagle".into(),
                playing: false,
                available: false,
            }
            .spawn()?,
        ))
    }
    pub fn update(&self, state: &PlayerState) {
        let title = state.track.as_ref().map_or_else(
            || "eagle".to_owned(),
            |track| format!("eagle — {}", track.title),
        );
        let playing = state.status == State::Playing;
        let available = state.track.is_some();
        self.0.update(move |tray| {
            tray.title = title;
            tray.playing = playing;
            tray.available = available;
        });
    }
}

/// The tray glyph from the brand assets: a black template on macOS, white elsewhere.
#[cfg(target_os = "macos")]
const GLYPH: &[u8] = include_bytes!("../assets/brand/tray-macos.png");
#[cfg(not(target_os = "macos"))]
const GLYPH: &[u8] = include_bytes!("../assets/brand/tray-white.png");

/// RGBA pixels of the glyph with its width and height.
fn icon() -> Result<(Vec<u8>, u32, u32), image::ImageError> {
    let glyph = image::load_from_memory_with_format(GLYPH, image::ImageFormat::Png)?.to_rgba8();
    let (width, height) = glyph.dimensions();
    Ok((glyph.into_raw(), width, height))
}

#[cfg(test)]
mod tests {
    #[test]
    fn glyph_decodes_to_visible_pixels() {
        let (pixels, width, height) = super::icon().unwrap();
        assert_eq!(pixels.len(), (width * height * 4) as usize);
        assert!(width >= 16 && width == height);
        assert!(pixels.as_chunks::<4>().0.iter().any(|p| p[3] > 200));
        assert!(pixels.as_chunks::<4>().0.iter().any(|p| p[3] == 0));
    }
}
