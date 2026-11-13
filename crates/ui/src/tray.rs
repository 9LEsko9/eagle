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
        let icon = tray_icon::Icon::from_rgba(icon(), 16, 16)?;
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
        let mut pixels = icon();
        for pixel in pixels.as_chunks_mut::<4>().0 {
            pixel.rotate_right(1);
        }
        vec![ksni::Icon {
            width: 16,
            height: 16,
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

fn icon() -> Vec<u8> {
    let mut pixels = vec![0; 16 * 16 * 4];
    // Monochrome E mark, suitable for macOS template rendering.
    for y in 2..14 {
        for x in 3..13 {
            if x < 5 || y < 4 || ((7..9).contains(&y) && x < 11) || y >= 12 {
                let i = (y * 16 + x) * 4;
                #[cfg(target_os = "macos")]
                let color = [0, 0, 0, 255];
                #[cfg(not(target_os = "macos"))]
                let color = [255, 255, 255, 255];
                pixels[i..i + 4].copy_from_slice(&color);
            }
        }
    }
    pixels
}
