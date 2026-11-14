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
    },
    time::Instant,
};
slint::include_modules!();
mod bridge;
mod events;
#[cfg(test)]
mod grid_tests;
mod instance;
mod media;
#[cfg(target_os = "windows")]
mod native_window;
mod notification;
mod preferences;
mod renderer;
pub use renderer::Renderer;
mod smoke;
mod tray;
#[cfg(target_os = "macos")]
mod window_chrome;

use eagle_app::session::{Command, CommandSender, Event};
use std::cell::RefCell;
use std::sync::atomic::AtomicU64;

#[derive(Clone, Default)]
struct QueueEditorState {
    open: bool,
    focused: slint::SharedString,
    scroll_y: f32,
}
impl QueueEditorState {
    fn capture(ui: &DesktopWindow) -> Self {
        let state = ui.global::<Ui>();
        Self {
            open: state.get_queue_editor_open(),
            focused: state.get_queue_focused_occurrence(),
            scroll_y: state.get_queue_scroll_y(),
        }
    }
    fn apply(&self, ui: &DesktopWindow) {
        let state = ui.global::<Ui>();
        state.set_queue_focused_occurrence(self.focused.clone());
        state.set_queue_editor_open(self.open);
        state.set_queue_scroll_y(self.scroll_y);
    }
}
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
    renderer: Renderer,
    window: Option<DesktopWindow>,
    waiting_snapshot: bool,
    queue_editor: QueueEditorState,
    storage_locks: Vec<(
        PathBuf,
        Arc<eagle_store::relocation::StorageLease>,
        instance::Activation,
    )>,
}
thread_local! { static SHELL: RefCell<Option<Shell>> = const { RefCell::new(None) }; }

struct Dispatch {
    batch: Mutex<events::Batch>,
    media: Mutex<Option<media::Handle>>,
    anchor: Arc<Mutex<Option<PositionAnchor>>>,
    visible: Arc<AtomicBool>,
    open: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    pending: Arc<AtomicU64>,
    closed: AtomicBool,
}
impl Dispatch {
    fn send(self: &Arc<Self>, event: Event) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        if let Some(media) = self
            .media
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            media.event(&event);
        }
        #[allow(deprecated)]
        let last_snapshot = matches!(&event, Event::Snapshot(_))
            && self
                .pending
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
                .ok()
                == Some(1);
        let Some(event) = bridge::coalesce_hidden_position(
            event,
            self.visible.load(Ordering::Acquire),
            &self.anchor,
        ) else {
            return;
        };
        if !self.open.load(Ordering::Acquire)
            && !matches!(&event, Event::Player(_) | Event::Storage { .. })
        {
            return;
        }
        let schedule = {
            let mut batch = self.batch.lock().unwrap_or_else(|e| e.into_inner());
            batch.push(events::Envelope {
                event,
                epoch: self.generation.load(Ordering::Acquire),
                last_snapshot,
            });
            if batch.scheduled {
                false
            } else {
                batch.scheduled = true;
                true
            }
        };
        if schedule {
            self.schedule();
        }
    }
    fn schedule(self: &Arc<Self>) {
        let dispatch = self.clone();
        if slint::invoke_from_event_loop(move || dispatch.drain()).is_err() {
            self.closed.store(true, Ordering::Release);
            *self.batch.lock().unwrap_or_else(|e| e.into_inner()) = events::Batch::default();
        }
    }
    fn drain(self: Arc<Self>) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        let started = Instant::now();
        while started.elapsed() < std::time::Duration::from_millis(2) {
            let next = self.batch.lock().unwrap_or_else(|e| e.into_inner()).pop();
            let Some(events::Envelope {
                event,
                epoch,
                last_snapshot,
            }) = next
            else {
                break;
            };
            if let Event::Storage { folder, .. } = &event {
                update_storage_root(folder.clone());
            }
            SHELL.with(|shell| {
                let mut shell = shell.borrow_mut();
                let Some(shell) = shell.as_mut() else {
                    return;
                };
                let player = match &event {
                    Event::Player(player) => Some(player.clone()),
                    Event::Snapshot(snapshot) => Some(snapshot.player.clone()),
                    _ => None,
                };
                if let Some(player) = player {
                    if let Some(tray) = &shell.tray {
                        tray.update(&player);
                    }
                    shell.player = Some(player);
                }
                let restoring = shell.waiting_snapshot && matches!(&event, Event::Snapshot(_));
                if !shell.open.load(Ordering::Acquire)
                    || !accept_event(
                        &mut shell.waiting_snapshot,
                        shell.generation.load(Ordering::Acquire) == epoch,
                        matches!(&event, Event::Snapshot(_)),
                        last_snapshot,
                    )
                {
                    return;
                }
                if let Some(ui) = &shell.window {
                    bridge::deliver(ui, event);
                    if restoring {
                        shell.queue_editor.apply(ui);
                    }
                }
            });
        }
        let (again, retry) = {
            let mut batch = self.batch.lock().unwrap_or_else(|e| e.into_inner());
            let retry = std::mem::take(&mut batch.retry_artwork);
            let again = !batch.is_empty();
            batch.scheduled = again;
            (again, retry)
        };
        if retry {
            SHELL.with(|shell| {
                if let Some(ui) = shell
                    .borrow()
                    .as_ref()
                    .and_then(|shell| shell.window.as_ref())
                {
                    bridge::retry_artwork(ui);
                }
            });
        }
        if again {
            self.schedule();
        }
    }
}

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

// Recovery may select a directory outside both initially locked roots.
fn claim_resolved(
    root: &std::path::Path,
    original: &std::path::Path,
    bootstrap: &std::path::Path,
    primaries: &mut Vec<instance::Primary>,
) -> std::io::Result<bool> {
    let resolved = std::fs::canonicalize(root)?;
    for path in [original, bootstrap] {
        if std::fs::canonicalize(path)? == resolved {
            return Ok(true);
        }
    }
    match instance::Instance::claim(root)? {
        instance::Instance::Primary(primary) => {
            primaries.push(primary);
            Ok(true)
        }
        instance::Instance::Activated => Ok(false),
    }
}

pub fn run(root: PathBuf, renderer: Renderer) -> Result<(), Box<dyn std::error::Error>> {
    run_inner(root.clone(), root, renderer, None)
}
pub fn run_with_options(
    root: PathBuf,
    bootstrap_root: PathBuf,
    renderer: Renderer,
) -> Result<(), Box<dyn std::error::Error>> {
    run_inner(root, bootstrap_root, renderer, None)
}
pub fn run_with_providers(
    root: PathBuf,
    renderer: Renderer,
    providers: Vec<Arc<dyn eagle_core::Provider>>,
) -> Result<(), Box<dyn std::error::Error>> {
    run_inner(root.clone(), root, renderer, Some(providers))
}
pub fn run_with_providers_and_bootstrap(
    root: PathBuf,
    bootstrap_root: PathBuf,
    renderer: Renderer,
    providers: Vec<Arc<dyn eagle_core::Provider>>,
) -> Result<(), Box<dyn std::error::Error>> {
    run_inner(root, bootstrap_root, renderer, Some(providers))
}
fn run_inner(
    root: PathBuf,
    bootstrap_root: PathBuf,
    renderer: Renderer,
    providers: Option<Vec<Arc<dyn eagle_core::Provider>>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(mut primaries) = claim_instances(&root, &bootstrap_root)? else {
        return Ok(());
    };
    let original_root = root.clone();
    let root = if providers.is_none() {
        eagle_app::resolve_storage(root, &bootstrap_root)?
    } else {
        root
    };
    if providers.is_none()
        && !claim_resolved(&root, &original_root, &bootstrap_root, &mut primaries)?
    {
        return Ok(());
    }
    renderer.select()?;
    let anchor = Arc::new(Mutex::new(None::<PositionAnchor>));
    let visible = Arc::new(AtomicBool::new(false));
    let open = Arc::new(AtomicBool::new(false));
    let generation = Arc::new(AtomicU64::new(0));
    let pending = Arc::new(AtomicU64::new(0));
    let dispatch = Arc::new(Dispatch {
        batch: Mutex::new(events::Batch::default()),
        media: Mutex::new(None),
        anchor: anchor.clone(),
        visible: visible.clone(),
        open: open.clone(),
        generation: generation.clone(),
        pending: pending.clone(),
        closed: AtomicBool::new(false),
    });
    let delivery = dispatch.clone();
    let emit = move |event| delivery.send(event);
    let notify_on_close = providers.is_none();
    let session = match providers {
        Some(providers) => Session::start_with_providers(root.clone(), providers, emit)?,
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
            renderer,
            window: None,
            waiting_snapshot: false,
            queue_editor: QueueEditorState::default(),
            storage_locks: Vec::new(),
        })
    });
    *dispatch.media.lock().unwrap_or_else(|e| e.into_inner()) = Some(media.handle());
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
    let storage_locks = SHELL.with(|shell| {
        let shell = shell.borrow_mut().take();
        if let Some(shell) = &shell
            && shell.window.is_some()
        {
            let _ = shell.sender.send(Command::UiAttached(false));
        }
        shell.map(|shell| shell.storage_locks)
    });
    bridge::clear();
    dispatch.closed.store(true, Ordering::Release);
    drop(session);
    drop(activations);
    drop(storage_locks);
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
    let (root, sender, anchor, visible, fixture, renderer) = SHELL
        .with(|shell| {
            let shell = shell.borrow();
            let shell = shell.as_ref()?;
            Some((
                shell.root.clone(),
                shell.sender.clone(),
                shell.anchor.clone(),
                shell.visible.clone(),
                shell.fixture,
                shell.renderer,
            ))
        })
        .ok_or("window shell unavailable")?;
    let ui = DesktopWindow::new().map_err(|error| {
        format!(
            "Unable to create {} window: {error}. Try --software.",
            renderer.name()
        )
    })?;
    // Generated Slint components register their bundled translations on construction.
    slint::select_bundled_translation("ru")?;
    if renderer == Renderer::Software {
        log::info!(target: "eagle_ui", "rendering API: software");
    } else {
        let gpu_root = root.clone();
        if ui
            .window()
            .set_rendering_notifier(move |state, api| {
                if matches!(state, slint::RenderingState::RenderingSetup) {
                    let api = match api {
                        slint::GraphicsAPI::WGPU29 { device, .. } => {
                            format!("WGPU/{:?}", device.adapter_info().backend)
                        }
                        slint::GraphicsAPI::NativeOpenGL { .. } => "OpenGL".to_owned(),
                        slint::GraphicsAPI::WebGL { .. } => "WebGL".to_owned(),
                        _ => "unknown".to_owned(),
                    };
                    log::info!(target: "eagle_ui", "rendering API: {api}");
                    if fixture {
                        let _ = std::fs::write(
                            gpu_root.join("fixture-gpu.json"),
                            serde_json::json!({"actual_graphics_api": api}).to_string(),
                        );
                    }
                }
            })
            .is_err()
        {
            log::info!(target: "eagle_ui", "rendering API: unknown");
        }
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
            let mut shell = shell.borrow_mut();
            let Some(shell) = shell.as_mut() else {
                return;
            };
            if shell.fixture || shell.root.join(".eagle-fixture").exists() {
                if let Some(ui) = weak.upgrade() {
                    ui.global::<Ui>()
                        .set_error("Перенос недоступен в тестовом режиме".into());
                }
                return;
            }
            let destination = PathBuf::from(destination.as_str());
            let result = (|| -> Result<(), Box<dyn std::error::Error>> {
                let canonical = std::fs::canonicalize(&destination)?;
                let lease = if let Some((_, lease, _)) = shell
                    .storage_locks
                    .iter()
                    .find(|(path, _, _)| *path == canonical)
                {
                    lease.clone()
                } else {
                    let (primary, lease) = instance::Primary::claim_empty(&canonical)?;
                    let activation = primary.listen(|| {
                        let _ = slint::invoke_from_event_loop(|| {
                            let _ = show_window();
                        });
                    })?;
                    shell
                        .storage_locks
                        .push((canonical, lease.clone(), activation));
                    lease
                };
                shell
                    .sender
                    .send(Command::RelocateStorage {
                        destination,
                        bootstrap: shell.bootstrap_root.clone(),
                        lease: Some(lease),
                    })
                    .map_err(|_| "session stopped")?;
                Ok(())
            })();
            if let Err(error) = result
                && let Some(ui) = weak.upgrade()
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
                #[cfg(target_os = "macos")]
                window_chrome::apply(&ui);
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
                if let Some(ui) = shell.window.take() {
                    if !shell.waiting_snapshot {
                        shell.queue_editor = QueueEditorState::capture(&ui);
                    }
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
        return Err(format!(
            "Unable to show {} window: {error}. Try --software.",
            renderer.name()
        )
        .into());
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
fn record_fixture_window(root: &std::path::Path, stage: &str) {
    let value = SHELL.with(|shell| {
        let shell = shell.borrow();
        let shell = shell.as_ref()?;
        let size = shell.window.as_ref().map(|ui| ui.window().size());
        Some(serde_json::json!({
            "stage": stage,
            "surface_physical_pixels": size.map(|size| [size.width, size.height]),
            "scale_factor": shell.window.as_ref().map(|ui| ui.window().scale_factor()),
            "window_open": shell.open.load(Ordering::Acquire),
            "visible_to_user": shell.visible.load(Ordering::Acquire),
        }))
    });
    if let Some(value) = value {
        let _ = std::fs::write(root.join("fixture-window.json"), value.to_string());
    }
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
    fn recovered_storage_is_locked_before_session_starts() {
        let base =
            std::env::temp_dir().join(format!("eagle-recovered-lock-{}", std::process::id()));
        let (original, bootstrap, active) = (
            base.join("original"),
            base.join("bootstrap"),
            base.join("active"),
        );
        std::fs::create_dir_all(&active).unwrap();
        let mut primaries = claim_instances(&original, &bootstrap).unwrap().unwrap();
        assert!(claim_resolved(&active, &original, &bootstrap, &mut primaries).unwrap());
        assert!(matches!(
            instance::Instance::claim(&active).unwrap(),
            instance::Instance::Activated
        ));
        drop(primaries);
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
    fn player_bar_centre_is_independent_of_title() {
        let ui = DesktopWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(1280., 880.));
        ui.global::<Player>().set_title("A".into());
        ui.global::<Player>().set_artist("A".into());
        ui.show().unwrap();
        let play_x = || {
            ElementHandle::find_by_accessible_label(&ui, "Играть")
                .next()
                .unwrap()
                .absolute_position()
                .x
        };
        let short = play_x();
        ui.global::<Player>().set_title("A".repeat(200).into());
        ui.global::<Player>().set_artist("A".repeat(200).into());
        let long = play_x();
        assert!((short - long).abs() <= 0.5, "short={short}, long={long}");
        drop(ui);
        for width in [720., 834., 835.] {
            let ui = DesktopWindow::new().unwrap();
            ui.window().set_size(slint::LogicalSize::new(width, 880.));
            ui.global::<Player>().set_title("A".repeat(200).into());
            ui.show().unwrap();
            let play = ElementHandle::find_by_accessible_label(&ui, "Играть")
                .next()
                .unwrap();
            let centre = play.absolute_position().x + play.size().width / 2.;
            assert!(
                (centre - width / 2.).abs() <= 2.5,
                "width={width}, centre={centre}"
            );
        }
    }
    #[test]
    fn accessible_actions_delta_and_hidden_player_timer() {
        i_slint_backend_testing::init_integration_test_with_mock_time();
        player_bar_centre_is_independent_of_title();
        for width in [1000., 1067., 1068., 1127., 1128., 1129., 1280.] {
            let panel_ui = DesktopWindow::new().unwrap();
            panel_ui
                .window()
                .set_size(slint::LogicalSize::new(width, 880.));
            panel_ui.global::<Ui>().set_signed_in(true);
            panel_ui.global::<Ui>().set_route(Route::Playlist);
            panel_ui.show().unwrap();
            let check_panel = |width: f32| {
                for surface in
                    ElementHandle::find_by_element_id(&panel_ui, "NowPlayingCard::panel-surface")
                {
                    let right = surface.absolute_position().x + surface.size().width;
                    assert!(right <= width + 0.1, "width={width}, panel right={right}");
                }
            };
            for index in 0..5 {
                let label = if index % 2 == 0 {
                    "Показать панель"
                } else {
                    "Скрыть панель"
                };
                ElementHandle::find_by_accessible_label(&panel_ui, label)
                    .next()
                    .unwrap()
                    .invoke_accessible_default_action();
                i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(200));
                slint::platform::update_timers_and_animations();
                check_panel(width);
            }
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(500));
            slint::platform::update_timers_and_animations();
            assert!(panel_ui.global::<Ui>().get_panel_open());
            assert_eq!(
                ElementHandle::find_by_element_id(&panel_ui, "NowPlayingCard::panel-surface")
                    .count(),
                1
            );
            check_panel(width);
            for resized in [1067., 1068., 1128.] {
                panel_ui
                    .window()
                    .set_size(slint::LogicalSize::new(resized, 880.));
                slint::platform::update_timers_and_animations();
                check_panel(resized);
            }
            panel_ui
                .window()
                .set_size(slint::LogicalSize::new(width, 880.));
            slint::platform::update_timers_and_animations();
            let close = ElementHandle::find_by_accessible_label(&panel_ui, "Скрыть панель")
                .find(|element| element.absolute_position().y < 100.)
                .unwrap();
            let right = close.absolute_position().x + close.size().width;
            assert!(right <= width, "width={width}, close right={right}");
            assert!(
                ElementHandle::find_by_accessible_label(&panel_ui, "Скрыть панель").count() >= 2
            );
            ElementHandle::find_by_accessible_label(&panel_ui, "Скрыть панель")
                .next()
                .unwrap()
                .invoke_accessible_default_action();
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(500));
            slint::platform::update_timers_and_animations();
            assert!(!panel_ui.global::<Ui>().get_panel_open());
            assert_eq!(
                ElementHandle::find_by_accessible_label(&panel_ui, "Скрыть панель").count(),
                0
            );
        }

        let window = DesktopWindow::new().unwrap();
        let weak = window.as_weak();
        window.show().unwrap();
        window.hide().unwrap();
        drop(window);
        assert!(
            weak.upgrade().is_none(),
            "closed window must release its component"
        );
        let editor = QueueEditorState {
            open: true,
            focused: "17".into(),
            scroll_y: -336.,
        };
        let reopened = DesktopWindow::new().unwrap();
        editor.apply(&reopened);
        let restored = QueueEditorState::capture(&reopened);
        assert!(restored.open);
        assert_eq!(restored.focused, "17");
        assert_eq!(restored.scroll_y, -336.);
        reopened.show().unwrap();
        reopened.hide().unwrap();
        drop(reopened);
        bridge::tests::structural_changes_and_cover_toggle_release_artwork();
        grid_tests::visible_cards_keep_source_actions();
        let ui = DesktopWindow::new().unwrap();
        let rows = Rc::new(VecModel::from(vec![TrackItem {
            occurrence: Default::default(),
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
        let saved = Rc::new(RefCell::new(String::new()));
        ui.global::<Actions>().on_save({
            let saved = saved.clone();
            move |id| *saved.borrow_mut() = id.to_string()
        });
        ElementHandle::find_by_accessible_label(&ui, "Ещё")
            .next()
            .unwrap()
            .invoke_accessible_default_action();
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::KeyPressed { text: "\n".into() });
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::KeyReleased { text: "\n".into() });
        assert_eq!(&*saved.borrow(), "yandex:1");
        saved.borrow_mut().clear();
        ElementHandle::find_by_accessible_label(&ui, "Ещё")
            .next()
            .unwrap()
            .invoke_accessible_default_action();
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::KeyPressed { text: " ".into() });
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::KeyReleased { text: " ".into() });
        assert_eq!(&*saved.borrow(), "yandex:1");
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
        ElementHandle::find_by_accessible_label(&ui, "Играть трек")
            .next()
            .unwrap()
            .invoke_accessible_default_action();
        assert_eq!(&*played_rows.borrow(), &[0]);
        played_rows.borrow_mut().clear();
        ui.global::<Player>().set_track_id("yandex:1".into());
        ui.global::<Player>().set_status(Status::Playing);
        ElementHandle::find_by_accessible_label(&ui, "Приостановить трек")
            .next()
            .unwrap()
            .invoke_accessible_default_action();
        assert_eq!(*toggles.borrow(), 2);
        ui.global::<Player>().set_status(Status::Paused);
        ElementHandle::find_by_accessible_label(&ui, "Играть трек")
            .next()
            .unwrap()
            .invoke_accessible_default_action();
        assert_eq!(*toggles.borrow(), 3);
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
        ElementHandle::find_by_accessible_label(&ui, "Действия со списком")
            .next()
            .unwrap()
            .invoke_accessible_default_action();
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
