// SPDX-License-Identifier: GPL-3.0-or-later
use eagle_app::session::{Command, CommandSender, PlayerState, position_now};
use eagle_engine::{PositionAnchor, State};
use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
    SeekDirection,
};
use std::{
    cell::RefCell,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
thread_local! { static CONTROLS: RefCell<Option<MediaControls>> = const { RefCell::new(None) }; }
pub fn init(
    sender: CommandSender,
    anchor: Arc<Mutex<Option<PositionAnchor>>>,
    hwnd: Option<*mut std::ffi::c_void>,
    window: slint::Weak<crate::DesktopWindow>,
) -> Result<(), souvlaki::Error> {
    let mut controls = MediaControls::new(PlatformConfig {
        display_name: "eagle",
        dbus_name: "eagle",
        hwnd,
    })?;
    controls.attach(move |event| {
        let position = || {
            anchor
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .as_ref()
                .map_or(Duration::ZERO, |a| position_now(a, Instant::now()))
        };
        let command = match event {
            MediaControlEvent::Play => Command::Resume,
            MediaControlEvent::Pause => Command::Pause,
            MediaControlEvent::Toggle => Command::TogglePlay,
            MediaControlEvent::Next => Command::Next,
            MediaControlEvent::Previous => Command::Previous,
            MediaControlEvent::Stop => Command::Stop,
            MediaControlEvent::SetPosition(p) => Command::Seek(p.0),
            MediaControlEvent::SetShuffle(shuffle) => Command::SetShuffle(shuffle),
            MediaControlEvent::Quit => {
                let _ = slint::quit_event_loop();
                return;
            }
            MediaControlEvent::Raise => {
                use slint::ComponentHandle;
                let window = window.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = window.upgrade() {
                        use slint::winit_030::WinitWindowAccessor;
                        let _ = ui.show();
                        ui.window().with_winit_window(|w| {
                            w.set_minimized(false);
                            w.focus_window();
                        });
                    }
                });
                return;
            }
            MediaControlEvent::SetVolume(v) => Command::Volume(v as f32),
            MediaControlEvent::SeekBy(direction, amount) => Command::Seek(match direction {
                SeekDirection::Forward => position().saturating_add(amount),
                SeekDirection::Backward => position().saturating_sub(amount),
            }),
            MediaControlEvent::Seek(direction) => Command::Seek(match direction {
                SeekDirection::Forward => position().saturating_add(Duration::from_secs(5)),
                SeekDirection::Backward => position().saturating_sub(Duration::from_secs(5)),
            }),
            _ => return,
        };
        let _ = sender.send(command);
    })?;
    CONTROLS.with(|c| *c.borrow_mut() = Some(controls));
    Ok(())
}
pub fn state(state: &PlayerState) {
    CONTROLS.with(|c| {
        let mut c = c.borrow_mut();
        let Some(c) = c.as_mut() else {
            return;
        };
        if let Some(track) = &state.track {
            let artists = track
                .artists
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let _ = c.set_metadata(MediaMetadata {
                title: Some(&track.title),
                artist: Some(&artists),
                duration: Some(Duration::from_millis(track.duration_ms)),
                ..Default::default()
            });
        }
        let _ = c.set_playback(match state.status {
            State::Playing => MediaPlayback::Playing { progress: None },
            State::Paused | State::Loading | State::Buffering => {
                MediaPlayback::Paused { progress: None }
            }
            State::Idle => MediaPlayback::Stopped,
        });
        #[cfg(target_os = "macos")]
        let _ = c.set_shuffle(Some(state.shuffle));
        #[cfg(all(
            unix,
            not(any(target_os = "macos", target_os = "ios", target_os = "android"))
        ))]
        let _ = c.set_volume(f64::from(state.volume));
    });
}
pub fn position(anchor: &PositionAnchor) {
    CONTROLS.with(|c| {
        if let Some(c) = c.borrow_mut().as_mut() {
            let progress = Some(MediaPosition(position_now(anchor, Instant::now())));
            let _ = c.set_playback(if anchor.state == State::Playing {
                MediaPlayback::Playing { progress }
            } else {
                MediaPlayback::Paused { progress }
            });
        }
    });
}
pub fn clear() {
    CONTROLS.with(|c| {
        c.borrow_mut().take();
    });
}
