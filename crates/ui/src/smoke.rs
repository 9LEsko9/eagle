// SPDX-License-Identifier: GPL-3.0-or-later
//! Opt-in deterministic release measurements against an isolated fixture root.
use eagle_app::session::{Command, View};
use slint::ComponentHandle;
use std::{path::PathBuf, sync::mpsc, thread, time::Duration};

thread_local! { static SCROLL: slint::Timer = slint::Timer::default(); }

fn start_scrolling(step: f32, offscreen: bool) {
    let mut tick = 0_u32;
    SCROLL.with(|timer| {
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(16),
            move || {
                crate::SHELL.with(|shell| {
                    let shell = shell.borrow();
                    let Some(ui) = shell.as_ref().and_then(|shell| shell.window.as_ref()) else {
                        return;
                    };
                    let size = ui.window().size();
                    let scale = ui.window().scale_factor();
                    ui.window()
                        .dispatch_event(slint::platform::WindowEvent::PointerScrolled {
                            position: slint::LogicalPosition::new(
                                size.width as f32 / scale * 0.55,
                                size.height as f32 / scale * 0.5,
                            ),
                            delta_x: 0.,
                            delta_y: if (tick / 60).is_multiple_of(2) {
                                -step
                            } else {
                                step
                            },
                        });
                    if offscreen && let Err(error) = ui.window().take_snapshot() {
                        eprintln!("Eagle fixture scroll snapshot failed: {error}");
                        let _ = slint::quit_event_loop();
                    }
                });
                tick += 1;
            },
        );
    });
}

pub(crate) struct Smoke {
    stop: mpsc::Sender<()>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Smoke {
    pub(crate) fn start(root: PathBuf, scroll: bool) -> std::io::Result<Self> {
        let offscreen = std::env::var("EAGLE_FIXTURE_SCROLL_OFFSCREEN").is_ok_and(|v| v == "1");
        let scroll_step = if scroll {
            match std::env::var("EAGLE_FIXTURE_SCROLL_STEP") {
                Ok(value) => value
                    .parse::<std::num::NonZeroU16>()
                    .map_err(std::io::Error::other)?
                    .get() as f32,
                Err(std::env::VarError::NotPresent) => 512.,
                Err(error) => return Err(std::io::Error::other(error)),
            }
        } else {
            512.
        };
        let (stop, wait) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("eg-fixture-smoke".into())
            .spawn(move || {
                for stage in [
                    "idle",
                    "scrolling",
                    "playing",
                    "closed-playing",
                    "reopened-playing",
                    "paused",
                    "closed",
                    "reopened",
                    "closed-again",
                    "reopened-again",
                    "quit",
                ] {
                    if stage == "scrolling" && !scroll {
                        continue;
                    }
                    if wait.recv_timeout(Duration::from_secs(6)).is_ok() {
                        return;
                    }
                    let root = root.clone();
                    if slint::invoke_from_event_loop(move || {
                        if scroll {
                            SCROLL.with(|timer| timer.stop());
                            eprintln!("Eagle fixture stage: {stage}");
                        }
                        match stage {
                            "idle" => crate::fixture_command(Command::Open(View::Saved)),
                            "scrolling" => start_scrolling(scroll_step, offscreen),
                            "playing" => {
                                crate::fixture_command(Command::Volume(0.05));
                                crate::fixture_command(Command::Resume);
                            }
                            "paused" => crate::fixture_command(Command::Pause),
                            "closed" | "closed-again" | "closed-playing" => crate::close_window(),
                            "reopened" | "reopened-again" | "reopened-playing" => {
                                let _ = crate::show_window();
                            }
                            "quit" => {
                                let _ = slint::quit_event_loop();
                            }
                            _ => {}
                        }
                        crate::record_fixture_window(&root, stage);
                        for delay in [500, 2_000, 4_000] {
                            let settled_root = root.clone();
                            slint::Timer::single_shot(Duration::from_millis(delay), move || {
                                crate::record_fixture_window(&settled_root, stage);
                            });
                        }
                        if let Err(error) = std::fs::write(root.join("smoke-stage"), stage) {
                            log::warn!("fixture smoke marker: {error}");
                        }
                    })
                    .is_err()
                    {
                        return;
                    }
                }
            })?;
        Ok(Self {
            stop,
            worker: Some(worker),
        })
    }
}
impl Drop for Smoke {
    fn drop(&mut self) {
        SCROLL.with(|timer| timer.stop());
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
