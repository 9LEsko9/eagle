// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(not(any(target_os = "android", target_os = "ios")))]
//! Compiled Slint desktop interface. Only this crate touches UI globals.
use eagle_app::session::Session;
use eagle_engine::PositionAnchor;
use slint::{ComponentHandle, ModelRc, VecModel};
use std::{
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Instant,
};
slint::include_modules!();
mod bridge;
mod instance;
mod media;
#[cfg(target_os = "windows")]
mod native_window;
mod notification;
mod preferences;
mod smoke;
mod tray;

use eagle_app::session::{Command, CommandSender, Event};
use std::cell::RefCell;
use std::sync::atomic::AtomicU64;

struct Shell {
    root: PathBuf,
    bootstrap_root: PathBuf,
    sender: CommandSender,
    anchor: Arc<Mutex<Option<PositionAnchor>>>,
    visible: Arc<AtomicBool>,
    open: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    pending: Arc<AtomicU64>,
    tray: Option<tray::Tray>,
    player: Option<Arc<eagle_app::session::PlayerState>>,
    notify_on_close: bool,
    fixture: bool,
    software: bool,
    window: Option<DesktopWindow>,
    waiting_snapshot: bool,
}
thread_local! { static SHELL: RefCell<Option<Shell>> = const { RefCell::new(None) }; }

fn claim_instances(
    root: &std::path::Path,
    bootstrap: &std::path::Path,
) -> std::io::Result<Option<Vec<instance::Primary>>> {
    let mut primaries = Vec::new();
    for path in [Some(bootstrap), (root != bootstrap).then_some(root)]
        .into_iter()
        .flatten()
    {
        match instance::Instance::claim(path)? {
            instance::Instance::Activated => return Ok(None),
            instance::Instance::Primary(primary) => primaries.push(primary),
        }
    }
    Ok(Some(primaries))
}

pub fn run(root: PathBuf, software: bool) -> Result<(), Box<dyn std::error::Error>> {
    run_inner(root.clone(), root, software, None)
}
pub fn run_with_options(
    root: PathBuf,
    bootstrap_root: PathBuf,
    software: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    run_inner(root, bootstrap_root, software, None)
}
pub fn run_with_provider(
    root: PathBuf,
    software: bool,
    provider: Arc<dyn eagle_core::Provider>,
) -> Result<(), Box<dyn std::error::Error>> {
    run_inner(root.clone(), root, software, Some(provider))
}
pub fn run_with_provider_and_bootstrap(
    root: PathBuf,
    bootstrap_root: PathBuf,
    software: bool,
    provider: Arc<dyn eagle_core::Provider>,
) -> Result<(), Box<dyn std::error::Error>> {
    run_inner(root, bootstrap_root, software, Some(provider))
}
fn run_inner(
    root: PathBuf,
    bootstrap_root: PathBuf,
    software: bool,
    provider: Option<Arc<dyn eagle_core::Provider>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(primaries) = claim_instances(&root, &bootstrap_root)? else {
        return Ok(());
    };
    let root = if provider.is_none() {
        eagle_app::resolve_storage(root, &bootstrap_root)?
    } else {
        root
    };
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name(if software { "software" } else { "skia" }.into())
        .select()?;
    let anchor = Arc::new(Mutex::new(None::<PositionAnchor>));
    let visible = Arc::new(AtomicBool::new(false));
    let open = Arc::new(AtomicBool::new(false));
    let generation = Arc::new(AtomicU64::new(0));
    let pending = Arc::new(AtomicU64::new(0));
    let (events, rx) = mpsc::channel();
    let emit = move |event| {
        let _ = events.send(event);
    };
    let notify_on_close = provider.is_none();
    let session = match provider {
        Some(provider) => Session::start_with_provider(root.clone(), provider, emit)?,
        None => Session::start(root.clone(), emit)?,
    };
    let sender = session.sender();
    let media = media::Owner::start(sender.clone(), anchor.clone(), || {
        let _ = slint::invoke_from_event_loop(|| {
            if let Err(error) = show_window() {
                log::warn!("open window: {error}");
            }
        });
    })?;
    #[cfg(target_os = "windows")]
    let native_window = native_window::NativeWindow::new()?;
    #[cfg(target_os = "windows")]
    media.handle().init(Some(native_window.handle()));
    #[cfg(not(target_os = "windows"))]
    media.handle().init(None);
    SHELL.with(|shell| {
        *shell.borrow_mut() = Some(Shell {
            root,
            bootstrap_root,
            sender,
            anchor: anchor.clone(),
            visible: visible.clone(),
            open: open.clone(),
            generation: generation.clone(),
            pending: pending.clone(),
            tray: None,
            player: None,
            notify_on_close,
            fixture: !notify_on_close,
            software,
            window: None,
            waiting_snapshot: false,
        })
    });
    let event_media = media.handle();
    let dispatcher = std::thread::Builder::new()
        .name("eg-ui-events".into())
        .spawn(move || {
            while let Ok(event) = rx.recv() {
                event_media.event(&event);
                if let Event::Storage { folder, .. } = &event {
                    let folder = folder.clone();
                    let _ = slint::invoke_from_event_loop(move || update_storage_root(folder));
                }
                let player = match &event {
                    Event::Player(player) => Some(player.clone()),
                    Event::Snapshot(snapshot) => Some(snapshot.player.clone()),
                    _ => None,
                };
                if let Some(player) = player {
                    let _ = slint::invoke_from_event_loop(move || {
                        SHELL.with(|shell| {
                            if let Some(shell) = shell.borrow_mut().as_mut() {
                                if let Some(tray) = &shell.tray {
                                    tray.update(&player);
                                }
                                shell.player = Some(player);
                            }
                        })
                    });
                }
                let snapshot = matches!(&event, Event::Snapshot(_));
                // fetch_update is available on our Rust 1.92 MSRV.
                #[allow(deprecated)]
                let last_snapshot = if snapshot {
                    pending
                        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
                        .ok()
                        == Some(1)
                } else {
                    false
                };
                let Some(event) = bridge::coalesce_hidden_position(
                    event,
                    visible.load(Ordering::Acquire),
                    &anchor,
                ) else {
                    continue;
                };
                if !open.load(Ordering::Acquire) {
                    continue;
                }
                let epoch = generation.load(Ordering::Acquire);
                if slint::invoke_from_event_loop(move || {
                    SHELL.with(|shell| {
                        let mut shell = shell.borrow_mut();
                        let Some(shell) = shell.as_mut() else {
                            return;
                        };
                        if !accept_event(
                            &mut shell.waiting_snapshot,
                            shell.generation.load(Ordering::Acquire) == epoch,
                            snapshot,
                            last_snapshot,
                        ) {
                            return;
                        }
                        if let Some(ui) = &shell.window {
                            bridge::deliver(ui, event);
                        }
                    });
                })
                .is_err()
                {
                    break;
                }
            }
        })?;
    show_window()?;
    slint::invoke_from_event_loop(|| {
        let sender = SHELL.with(|shell| shell.borrow().as_ref().map(|shell| shell.sender.clone()));
        if let Some(sender) = sender {
            match tray::Tray::new(sender) {
                Ok(tray) => SHELL.with(|shell| {
                    if let Some(shell) = shell.borrow_mut().as_mut() {
                        if let Some(player) = &shell.player {
                            tray.update(player);
                        }
                        shell.tray = Some(tray);
                    }
                }),
                Err(error) => log::warn!("system tray unavailable: {error}"),
            }
        }
    })?;
    let activations = primaries
        .into_iter()
        .map(|primary| {
            primary.listen(|| {
                let _ = slint::invoke_from_event_loop(|| {
                    if let Err(error) = show_window() {
                        log::warn!("open window: {error}");
                    }
                });
            })
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    let smoke_root = SHELL.with(|shell| shell.borrow().as_ref().map(|shell| shell.root.clone()));
    let _smoke = if std::env::args().any(|arg| arg == "--fixture-smoke") {
        let root = smoke_root.ok_or("fixture shell missing")?;
        if !root.join(".eagle-fixture").is_file() {
            return Err("smoke requires --fixture-root".into());
        }
        Some(smoke::Smoke::start(root)?)
    } else {
        None
    };
    let result = slint::run_event_loop_until_quit();
    SHELL.with(|shell| {
        if let Some(shell) = shell.borrow_mut().take()
            && shell.window.is_some()
        {
            let _ = shell.sender.send(Command::UiAttached(false));
        }
    });
    bridge::clear();
    drop(activations);
    drop(session);
    let _ = dispatcher.join();
    media.shutdown();
    result?;
    Ok(())
}

fn update_storage_root(root: PathBuf) {
    let window = SHELL.with(|shell| {
        let mut shell = shell.borrow_mut();
        let shell = shell.as_mut()?;
        if shell.root == root {
            return None;
        }
        shell.root = root;
        shell.open.store(false, Ordering::Release);
        shell.visible.store(false, Ordering::Release);
        shell.generation.fetch_add(1, Ordering::AcqRel);
        let window = shell.window.take();
        if window.is_some() {
            let _ = shell.sender.send(Command::UiAttached(false));
        }
        window
    });
    if let Some(window) = window {
        let _ = window.hide();
        drop(window);
        bridge::clear();
        if let Err(error) = show_window() {
            log::warn!("open window after storage move: {error}");
        }
    }
}

fn close_window() {
    let window = SHELL.with(|shell| {
        shell
            .borrow()
            .as_ref()
            .and_then(|shell| shell.window.as_ref().map(|ui| ui.clone_strong()))
    });
    if let Some(ui) = window {
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::CloseRequested);
    }
}

#[cfg(target_os = "macos")]
fn set_dock_visible(visible: bool) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

    let Some(main_thread) = MainThreadMarker::new() else {
        log::warn!("cannot change Dock visibility off the main thread");
        return;
    };
    let policy = if visible {
        NSApplicationActivationPolicy::Regular
    } else {
        NSApplicationActivationPolicy::Accessory
    };
    if !NSApplication::sharedApplication(main_thread).setActivationPolicy(policy) {
        log::warn!("failed to change Dock visibility");
    }
}

fn fixture_command(command: Command) {
    SHELL.with(|shell| {
        if let Some(shell) = shell.borrow().as_ref() {
            let _ = shell.sender.send(command);
        }
    });
}

fn accept_event(
    waiting: &mut bool,
    current_window: bool,
    snapshot: bool,
    last_snapshot: bool,
) -> bool {
    if !current_window || (snapshot && !last_snapshot) {
        return false;
    }
    if *waiting {
        if !snapshot {
            return false;
        }
        *waiting = false;
    }
    true
}

fn show_window() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "macos")]
    set_dock_visible(true);
    let existing = SHELL.with(|shell| {
        shell
            .borrow()
            .as_ref()
            .and_then(|s| s.window.as_ref().map(|ui| ui.clone_strong()))
    });
    if let Some(ui) = existing {
        use slint::winit_030::WinitWindowAccessor;
        ui.show()?;
        ui.window().with_winit_window(|w| {
            w.set_minimized(false);
            w.focus_window();
        });
        return Ok(());
    }
    let (root, sender, anchor, visible, fixture, software) = SHELL
        .with(|shell| {
            let shell = shell.borrow();
            let shell = shell.as_ref()?;
            Some((
                shell.root.clone(),
                shell.sender.clone(),
                shell.anchor.clone(),
                shell.visible.clone(),
                shell.fixture,
                shell.software,
            ))
        })
        .ok_or("window shell unavailable")?;
    let ui = DesktopWindow::new()?;
    // Generated Slint components register their bundled translations on construction.
    slint::select_bundled_translation("ru")?;
    if software {
        log::info!(target: "eagle_ui", "rendering API: software");
    } else if ui
        .window()
        .set_rendering_notifier(|state, api| {
            if matches!(state, slint::RenderingState::RenderingSetup) {
                let api = match api {
                    slint::GraphicsAPI::NativeOpenGL { .. } => "OpenGL",
                    slint::GraphicsAPI::WebGL { .. } => "WebGL",
                    _ => "unknown",
                };
                log::info!(target: "eagle_ui", "rendering API: {api}");
            }
        })
        .is_err()
    {
        log::info!(target: "eagle_ui", "rendering API: unknown");
    }
    let preferences = preferences::Preferences::load(&root);
    preferences.apply(&ui);
    let rows = Rc::new(VecModel::<TrackItem>::default());
    ui.global::<Library>()
        .set_tracks(ModelRc::from(rows.clone()));
    ui.global::<Library>()
        .set_playlists(Rc::new(VecModel::<CollectionItem>::default()).into());
    let cards = Rc::new(VecModel::<CardItem>::default());
    ui.global::<Library>()
        .set_cards(ModelRc::from(cards.clone()));
    bridge::apply_colors(&ui);
    ui.global::<Theme>().on_palette_changed({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                bridge::apply_colors(&ui);
            }
        }
    });
    match eagle_platform::autostart::enabled() {
        Ok(enabled) => ui.global::<Ui>().set_autostart(enabled),
        Err(error) => log::warn!("autostart status: {error}"),
    }
    let fixture = fixture || root.join(".eagle-fixture").exists();
    let weak = ui.as_weak();
    let autostart_busy = Arc::new(AtomicBool::new(false));
    ui.global::<Actions>().on_autostart(move |enabled| {
        if fixture {
            if let Some(ui) = weak.upgrade() {
                ui.global::<Ui>()
                    .set_error("Автозапуск недоступен в тестовом режиме".into());
            }
            return;
        }
        if autostart_busy.swap(true, Ordering::AcqRel) {
            return;
        }
        let busy = autostart_busy.clone();
        let worker_weak = weak.clone();
        let spawn = std::thread::Builder::new()
            .name("eg-autostart".into())
            .spawn(move || {
                let result = eagle_platform::autostart::set_enabled(enabled);
                let _ = slint::invoke_from_event_loop(move || {
                    busy.store(false, Ordering::Release);
                    if let Some(ui) = worker_weak.upgrade() {
                        match result {
                            Ok(()) => ui.global::<Ui>().set_autostart(enabled),
                            Err(error) => ui
                                .global::<Ui>()
                                .set_error(format!("Автозапуск: {error}").into()),
                        }
                    }
                });
            });
        if let Err(error) = spawn {
            autostart_busy.store(false, Ordering::Release);
            if let Some(ui) = weak.upgrade() {
                ui.global::<Ui>()
                    .set_error(format!("Автозапуск: {error}").into());
            }
        }
    });
    bridge::initialize(rows.clone(), cards, anchor.clone(), sender.clone());
    bridge::bind(&ui, sender.clone());
    let weak = ui.as_weak();
    ui.global::<Actions>().on_move_storage(move |destination| {
        SHELL.with(|shell| {
            let shell = shell.borrow();
            let Some(shell) = shell.as_ref() else {
                return;
            };
            if shell.fixture || shell.root.join(".eagle-fixture").exists() {
                if let Some(ui) = weak.upgrade() {
                    ui.global::<Ui>()
                        .set_error("Перенос недоступен в тестовом режиме".into());
                }
                return;
            }
            if let Err(error) = shell.sender.send(Command::RelocateStorage {
                destination: destination.as_str().into(),
                bootstrap: shell.bootstrap_root.clone(),
            }) && let Some(ui) = weak.upgrade()
            {
                ui.global::<Ui>().set_error(error.to_string().into());
            }
        });
    });
    let preferences_root = root.clone();
    let weak = ui.as_weak();
    ui.global::<Actions>().on_preferences_changed(move || {
        if let Some(ui) = weak.upgrade()
            && let Err(error) = preferences::Preferences::capture(&ui).save(&preferences_root)
        {
            log::warn!("window preferences: {error}");
        }
    });
    slint::invoke_from_event_loop({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                use slint::winit_030::WinitWindowAccessor;
                ui.window().with_winit_window(|w| {
                    if let (Some(x), Some(y)) = (preferences.x, preferences.y) {
                        let position = slint::winit_030::winit::dpi::PhysicalPosition::new(x, y);
                        if w.available_monitors().any(|m| {
                            let origin = m.position();
                            let size = m.size();
                            x >= origin.x
                                && y >= origin.y
                                && i64::from(x) + 100 < i64::from(origin.x) + i64::from(size.width)
                                && i64::from(y) + 100 < i64::from(origin.y) + i64::from(size.height)
                        }) {
                            w.set_outer_position(position);
                        }
                    }
                    if let Some(theme) = w.theme() {
                        ui.global::<Theme>()
                            .set_system_dark(theme == slint::winit_030::winit::window::Theme::Dark);
                    }
                });
            }
        }
    })?;
    let position_anchor = anchor.clone();
    ui.global::<Player>().on_position(move || {
        position_anchor
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .map_or(0., |a| {
                eagle_app::session::position_now(a, Instant::now()).as_secs_f32()
            })
    });
    use slint::winit_030::{EventResult, WinitWindowAccessor, winit};
    let weak = ui.as_weak();
    let window_anchor = anchor.clone();
    let mut occluded = false;
    let mut drawable = true;
    ui.window().on_winit_window_event(move |_, event| {
        if let Some(ui) = weak.upgrade() {
            match event {
                winit::event::WindowEvent::ThemeChanged(theme) => ui
                    .global::<Theme>()
                    .set_system_dark(*theme == winit::window::Theme::Dark),
                winit::event::WindowEvent::Occluded(hidden) => occluded = *hidden,
                winit::event::WindowEvent::Resized(size) => {
                    drawable = size.width > 0 && size.height > 0
                }
                winit::event::WindowEvent::CloseRequested
                | winit::event::WindowEvent::Destroyed => drawable = false,
                _ => {}
            }
            // Wayland cannot report minimized state; conservatively stop UI updates without focus.
            let available = ui
                .window()
                .with_winit_window(|w| {
                    w.is_minimized()
                        .map_or_else(|| w.has_focus(), |minimized| !minimized)
                })
                .unwrap_or(false);
            let now_visible = !occluded && drawable && available;
            let was_visible = visible.swap(now_visible, Ordering::AcqRel);
            ui.global::<Ui>().set_visible_to_user(now_visible);
            if now_visible
                && !was_visible
                && let Some(anchor) = window_anchor
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .as_ref()
            {
                ui.global::<Player>().set_shown(
                    eagle_app::session::position_now(anchor, Instant::now()).as_secs_f32(),
                );
            }
        }
        EventResult::Propagate
    });

    let preferences_root = root.clone();
    let weak = ui.as_weak();
    ui.window().on_close_requested(move || {
        let exit = weak
            .upgrade()
            .is_some_and(|ui| ui.global::<Ui>().get_close_exits());
        if let Some(ui) = weak.upgrade()
            && let Err(error) = preferences::Preferences::capture(&ui).save(&preferences_root)
        {
            log::warn!("window preferences: {error}");
        }
        if exit {
            let _ = slint::quit_event_loop();
        }
        #[cfg(target_os = "macos")]
        if !exit {
            set_dock_visible(false);
        }
        SHELL.with(|shell| {
            if let Some(shell) = shell.borrow_mut().as_mut() {
                if shell.notify_on_close && !exit {
                    shell.notify_on_close = false;
                    notification::first_close(shell.root.clone());
                }
                shell.open.store(false, Ordering::Release);
                shell.visible.store(false, Ordering::Release);
                shell.generation.fetch_add(1, Ordering::AcqRel);
                if shell.window.take().is_some() {
                    let _ = shell.sender.send(Command::UiAttached(false));
                }
            }
        });
        bridge::clear();
        slint::CloseRequestResponse::HideWindow
    });
    if let Err(error) = ui.show() {
        bridge::clear();
        #[cfg(target_os = "macos")]
        set_dock_visible(false);
        return Err(error.into());
    }
    SHELL.with(|shell| {
        if let Some(shell) = shell.borrow_mut().as_mut() {
            shell.window = Some(ui.clone_strong());
            shell.waiting_snapshot = true;
            shell.pending.fetch_add(1, Ordering::AcqRel);
            shell.generation.fetch_add(1, Ordering::AcqRel);
            shell.open.store(true, Ordering::Release);
            let _ = shell.sender.send(Command::UiAttached(true));
            if shell.sender.send(Command::Snapshot).is_err() {
                shell.pending.fetch_sub(1, Ordering::AcqRel);
                shell.waiting_snapshot = false;
            }
        }
    });
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use i_slint_backend_testing::ElementHandle;
    use slint::Model;
    use std::{cell::RefCell, time::Duration};
    #[test]
    fn legacy_instance_is_activated_and_bootstrap_lock_released() {
        let base = std::env::temp_dir().join(format!(
            "eagle-lock-upgrade-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let legacy = base.join("data");
        let bootstrap = base.join("bootstrap");
        let instance::Instance::Primary(old) = instance::Instance::claim(&legacy).unwrap() else {
            panic!("legacy instance was not primary");
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let old = old
            .listen(move || {
                let _ = tx.send(());
            })
            .unwrap();
        assert!(claim_instances(&legacy, &bootstrap).unwrap().is_none());
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let primaries = claim_instances(&bootstrap, &bootstrap).unwrap().unwrap();
        assert_eq!(primaries.len(), 1);
        drop(primaries);
        drop(old);
        std::fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn recreated_window_waits_for_latest_snapshot_and_drops_old_events() {
        let mut waiting = true;
        assert!(!accept_event(&mut waiting, false, true, true));
        assert!(!accept_event(&mut waiting, true, false, false));
        assert!(!accept_event(&mut waiting, true, true, false));
        assert!(waiting);
        assert!(accept_event(&mut waiting, true, true, true));
        assert!(!waiting);
        assert!(accept_event(&mut waiting, true, false, false));
        assert!(!accept_event(&mut waiting, false, false, false));
    }
    #[test]
    fn accessible_actions_delta_and_hidden_player_timer() {
        i_slint_backend_testing::init_integration_test_with_mock_time();
        let window = DesktopWindow::new().unwrap();
        let weak = window.as_weak();
        window.show().unwrap();
        window.hide().unwrap();
        drop(window);
        assert!(
            weak.upgrade().is_none(),
            "closed window must release its component"
        );
        let reopened = DesktopWindow::new().unwrap();
        reopened.show().unwrap();
        reopened.hide().unwrap();
        drop(reopened);
        bridge::tests::structural_changes_and_cover_toggle_release_artwork();
        let ui = DesktopWindow::new().unwrap();
        let rows = Rc::new(VecModel::from(vec![TrackItem {
            id: "yandex:1".into(),
            album_id: "".into(),
            album: "".into(),
            cover: slint::Image::default(),
            title: "Первый трек".into(),
            artist: "Исполнитель".into(),
            duration: "1:00".into(),
            available: true,
            blocked_reason: "".into(),
            liked: false,
        }]));
        ui.global::<Library>().set_tracks(rows.clone().into());
        let liked = Rc::new(RefCell::new(String::new()));
        ui.global::<Actions>().on_like({
            let liked = liked.clone();
            move |id| *liked.borrow_mut() = id.to_string()
        });
        ui.show().unwrap();
        ElementHandle::find_by_accessible_label(&ui, "Нравится")
            .next()
            .unwrap()
            .invoke_accessible_default_action();
        assert_eq!(&*liked.borrow(), "yandex:1");
        let toggles = Rc::new(RefCell::new(0));
        ui.global::<Actions>().on_toggle_play({
            let toggles = toggles.clone();
            move || *toggles.borrow_mut() += 1
        });
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::KeyPressed { text: " ".into() });
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::KeyReleased { text: " ".into() });
        assert_eq!(*toggles.borrow(), 1);
        let fixture = rows.row_data(0).unwrap();
        rows.push(fixture.clone());
        let played_rows = Rc::new(RefCell::new(Vec::new()));
        ui.global::<Actions>().on_play_row({
            let played_rows = played_rows.clone();
            move |index| played_rows.borrow_mut().push(index)
        });
        slint::platform::update_timers_and_animations();
        ElementHandle::find_by_accessible_label(&ui, "Первый трек, Исполнитель")
            .nth(1)
            .unwrap()
            .invoke_accessible_default_action();
        assert_eq!(&*played_rows.borrow(), &[1]);
        ui.global::<Library>()
            .set_tracks(Rc::new(VecModel::from(vec![fixture; 5000])).into());
        slint::platform::update_timers_and_animations();
        let visible_rows = ElementHandle::find_by_element_type_name(&ui, "TrackRowView").count();
        assert!(
            visible_rows > 0 && visible_rows < 100,
            "visible rows: {visible_rows}"
        );

        let searches = Rc::new(RefCell::new(Vec::new()));
        ui.global::<Actions>().on_search({
            let searches = searches.clone();
            let weak = ui.as_weak();
            move |query| {
                searches.borrow_mut().push((
                    weak.unwrap().global::<Ui>().get_search_kind(),
                    query.to_string(),
                ))
            }
        });
        ui.global::<Ui>().set_route(Route::Search);
        ui.global::<Ui>().set_search_query("Мира".into());
        slint::platform::update_timers_and_animations();
        ElementHandle::find_by_accessible_label(&ui, "Альбомы")
            .next()
            .unwrap()
            .invoke_accessible_default_action();
        assert_eq!(&*searches.borrow(), &[(2, "Мира".to_owned())]);
        assert_eq!(ui.global::<Ui>().get_search_query().as_str(), "Мира");
        let opened = Rc::new(RefCell::new(None));
        ui.global::<Actions>().on_collection({
            let opened = opened.clone();
            move |route, id| *opened.borrow_mut() = Some((route, id.to_string()))
        });
        ui.global::<Library>().set_cards(
            Rc::new(VecModel::from(vec![CardItem {
                id: "yandex:12".into(),
                key: "a:yandex:12".into(),
                title: "Альбом для проверки".into(),
                subtitle: "".into(),
                cover: slint::Image::default(),
                round: false,
                route: Route::Album,
            }]))
            .into(),
        );
        slint::platform::update_timers_and_animations();
        ElementHandle::find_by_accessible_label(&ui, "Альбом для проверки")
            .next()
            .unwrap()
            .invoke_accessible_default_action();
        assert_eq!(
            *opened.borrow(),
            Some((Route::Album, "yandex:12".to_owned()))
        );
        ui.global::<Actions>()
            .on_valid_playlist_name(|name| !name.trim().is_empty());
        let created = Rc::new(RefCell::new(String::new()));
        ui.global::<Actions>().on_create_playlist({
            let created = created.clone();
            move |name| *created.borrow_mut() = name.to_string()
        });
        ui.global::<Ui>().set_signed_in(true);
        slint::platform::update_timers_and_animations();
        ElementHandle::find_by_accessible_label(&ui, "Создать плейлист")
            .next()
            .unwrap()
            .invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        ui.global::<Ui>().set_playlist_name("   ".into());
        ElementHandle::find_by_accessible_label(&ui, "Создать")
            .next()
            .unwrap()
            .invoke_accessible_default_action();
        assert!(created.borrow().is_empty());
        ui.global::<Ui>().set_playlist_name("Вечер".into());
        ElementHandle::find_by_accessible_label(&ui, "Создать")
            .next()
            .unwrap()
            .invoke_accessible_default_action();
        assert_eq!(&*created.borrow(), "Вечер");
        ui.global::<Ui>().set_create_playlist(false);
        ui.global::<Ui>().set_route(Route::Playlist);
        ui.global::<Library>().set_title("Вечер".into());
        ui.global::<Library>().set_deletable(true);
        let deleted = Rc::new(RefCell::new(false));
        ui.global::<Actions>().on_delete_playlist({
            let deleted = deleted.clone();
            move || *deleted.borrow_mut() = true
        });
        slint::platform::update_timers_and_animations();
        ElementHandle::find_by_accessible_label(&ui, "Удалить плейлист")
            .next()
            .unwrap()
            .invoke_accessible_default_action();
        assert!(!*deleted.borrow());
        slint::platform::update_timers_and_animations();
        ElementHandle::find_by_accessible_label(&ui, "Удалить")
            .next()
            .unwrap()
            .invoke_accessible_default_action();
        assert!(*deleted.borrow());
        ui.global::<Library>().set_deletable(false);
        ui.global::<Ui>().set_route(Route::Settings);
        ElementHandle::find_by_accessible_label(&ui, "Бумага")
            .next()
            .unwrap()
            .invoke_accessible_default_action();
        assert_eq!(ui.global::<Theme>().get_mode(), ThemeMode::Paper);
        let positions = Rc::new(RefCell::new(0));
        ui.global::<Player>().on_position({
            let positions = positions.clone();
            move || {
                *positions.borrow_mut() += 1;
                2.
            }
        });
        ui.global::<Player>().set_status(Status::Playing);
        slint::platform::update_timers_and_animations();
        i_slint_backend_testing::mock_elapsed_time(Duration::from_secs(1));
        slint::platform::update_timers_and_animations();
        assert!(*positions.borrow() > 0);
        ui.global::<Ui>().set_visible_to_user(false);
        slint::platform::update_timers_and_animations();
        let before = *positions.borrow();
        i_slint_backend_testing::mock_elapsed_time(Duration::from_secs(10));
        slint::platform::update_timers_and_animations();
        assert_eq!(*positions.borrow(), before);
    }
}
