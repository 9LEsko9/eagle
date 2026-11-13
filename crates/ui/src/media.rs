// SPDX-License-Identifier: GPL-3.0-or-later
use eagle_app::session::{Command, CommandSender, Event, PlayerState, position_now};
use eagle_engine::{PositionAnchor, State};
use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
    SeekDirection,
};
use std::{
    cell::RefCell,
    sync::{Arc, Mutex, mpsc},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
enum Update {
    Init(Option<usize>),
    State(Arc<PlayerState>),
    Position(PositionAnchor),
    Shutdown,
}

#[derive(Clone)]
pub struct Handle(mpsc::Sender<Update>);

impl Handle {
    pub fn init(&self, hwnd: Option<usize>) {
        let _ = self.0.send(Update::Init(hwnd));
    }

    pub fn event(&self, event: &Event) {
        match event {
            Event::Player(player) => {
                let _ = self.0.send(Update::State(player.clone()));
            }
            Event::Position(position) => {
                let _ = self.0.send(Update::Position(position.clone()));
            }
            Event::Snapshot(snapshot) => {
                let _ = self.0.send(Update::State(snapshot.player.clone()));
                if let Some(position) = &snapshot.position {
                    let _ = self.0.send(Update::Position(position.clone()));
                }
            }
            _ => {}
        }
    }
}

pub struct Owner {
    handle: Handle,
    thread: JoinHandle<()>,
}

impl Owner {
    pub fn start(
        sender: CommandSender,
        anchor: Arc<Mutex<Option<PositionAnchor>>>,
        raise: impl Fn() + Send + 'static,
    ) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("eg-media".into())
            .spawn(move || {
                let mut latest_state: Option<Arc<PlayerState>> = None;
                let mut latest_position = None;
                let mut initialization = Some((sender, anchor, raise));
                while let Ok(update) = rx.recv() {
                    match update {
                        Update::Init(hwnd) => {
                            #[cfg(target_os = "windows")]
                            if hwnd.is_none() {
                                log::warn!(
                                    "system media controls unavailable: missing native window"
                                );
                                continue;
                            }
                            if let Some((sender, anchor, raise)) = initialization.take() {
                                if let Err(error) =
                                    init(sender, anchor, hwnd.map(|h| h as *mut _), raise)
                                {
                                    log::warn!("system media controls unavailable: {error:?}");
                                }
                                if let Some(player) = &latest_state {
                                    state(player);
                                }
                                if let Some(position) = &latest_position {
                                    position_update(position);
                                }
                            }
                        }
                        Update::State(player) => {
                            state(&player);
                            latest_state = Some(player);
                        }
                        Update::Position(position) => {
                            position_update(&position);
                            latest_position = Some(position);
                        }
                        Update::Shutdown => break,
                    }
                }
                clear();
            })?;
        Ok(Self {
            handle: Handle(tx),
            thread,
        })
    }

    pub fn handle(&self) -> Handle {
        self.handle.clone()
    }

    pub fn shutdown(self) {
        let _ = self.handle.0.send(Update::Shutdown);
        let _ = self.thread.join();
    }
}

thread_local! { static CONTROLS: RefCell<Option<MediaControls>> = const { RefCell::new(None) }; }
pub fn init(
    sender: CommandSender,
    anchor: Arc<Mutex<Option<PositionAnchor>>>,
    hwnd: Option<*mut std::ffi::c_void>,
    raise: impl Fn() + Send + 'static,
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
                raise();
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
fn state(state: &PlayerState) {
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
fn position_update(anchor: &PositionAnchor) {
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
