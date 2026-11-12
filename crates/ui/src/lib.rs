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
mod preferences;

pub fn run(root: PathBuf, software: bool) -> Result<(), Box<dyn std::error::Error>> {
    let primary = match instance::Instance::claim(&root)? {
        instance::Instance::Activated => return Ok(()),
        instance::Instance::Primary(primary) => primary,
    };
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name(if software { "software" } else { "skia" }.into())
        .select()?;
    let ui = DesktopWindow::new()?;
    slint::select_bundled_translation("ru")?;
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
    let anchor = Arc::new(Mutex::new(None::<PositionAnchor>));
    bridge::apply_colors(&ui);
    ui.global::<Theme>().on_palette_changed({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                bridge::apply_colors(&ui);
            }
        }
    });
    let visible = Arc::new(AtomicBool::new(true));
    let dispatch_visible = visible.clone();
    let dispatch_anchor = anchor.clone();
    let weak = ui.as_weak();
    // VecModel remains on the UI thread; events carry only domain data.
    let ui_rows = rows;
    let event_rows = ui_rows.clone();
    let event_anchor = anchor.clone();
    let (events, rx) = mpsc::channel();
    let session = Session::start(root.clone(), move |event| {
        let _ = events.send(event);
    })?;
    let dispatcher = std::thread::Builder::new()
        .name("eg-ui-events".into())
        .spawn(move || {
            while let Ok(event) = rx.recv() {
                let Some(event) = bridge::coalesce_hidden_position(
                    event,
                    dispatch_visible.load(Ordering::Acquire),
                    &dispatch_anchor,
                ) else {
                    continue;
                };
                let weak = weak.clone();
                if slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        bridge::deliver(&ui, event);
                    }
                })
                .is_err()
                {
                    break;
                }
            }
        })?;
    let sender = session.sender();
    bridge::initialize(event_rows.clone(), cards, event_anchor, sender.clone());
    bridge::bind(&ui, sender.clone());
    slint::invoke_from_event_loop({
        let weak = ui.as_weak();
        let anchor = anchor.clone();
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
                    #[cfg(target_os = "windows")]
                    let hwnd = {
                        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
                        match w.window_handle().map(|h| h.as_raw()) {
                            Ok(RawWindowHandle::Win32(h)) => {
                                Some(h.hwnd.get() as *mut std::ffi::c_void)
                            }
                            _ => None,
                        }
                    };
                    #[cfg(not(target_os = "windows"))]
                    let hwnd = None;
                    if let Err(error) = media::init(sender, anchor, hwnd, ui.as_weak()) {
                        log::warn!("system media controls unavailable: {error:?}");
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
    let activation = primary.listen({
        let weak = ui.as_weak();
        move || {
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    let _ = ui.show();
                    ui.window().with_winit_window(|w| {
                        w.set_minimized(false);
                        w.focus_window();
                    });
                }
            });
        }
    })?;
    let preferences_root = root.clone();
    let weak = ui.as_weak();
    ui.window().on_close_requested(move || {
        if let Some(ui) = weak.upgrade()
            && let Err(error) = preferences::Preferences::capture(&ui).save(&preferences_root)
        {
            log::warn!("window preferences: {error}");
        }
        slint::CloseRequestResponse::HideWindow
    });
    let result = ui.run();
    media::clear();
    drop(activation);
    drop(session);
    let _ = dispatcher.join();
    bridge::clear();
    result?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use i_slint_backend_testing::ElementHandle;
    use slint::Model;
    use std::{cell::RefCell, time::Duration};
    #[test]
    fn accessible_actions_delta_and_hidden_player_timer() {
        i_slint_backend_testing::init_integration_test_with_mock_time();
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
