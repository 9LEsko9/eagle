// SPDX-License-Identifier: GPL-3.0-or-later
//! Opt-in deterministic release measurements against an isolated fixture root.
use eagle_app::session::{Command, View};
use std::{path::PathBuf, sync::mpsc, thread, time::Duration};

pub(crate) struct Smoke {
    stop: mpsc::Sender<()>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Smoke {
    pub(crate) fn start(root: PathBuf) -> std::io::Result<Self> {
        let (stop, wait) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("eg-fixture-smoke".into())
            .spawn(move || {
                for stage in [
                    "idle",
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
                    if wait.recv_timeout(Duration::from_secs(6)).is_ok() {
                        return;
                    }
                    let root = root.clone();
                    if slint::invoke_from_event_loop(move || {
                        match stage {
                            "idle" => crate::fixture_command(Command::Open(View::Saved)),
                            "playing" => {
                                crate::fixture_command(Command::Volume(0.05));
                                if let Ok(id) = "fixture:1".parse() {
                                    crate::fixture_command(Command::Play(id));
                                }
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
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
