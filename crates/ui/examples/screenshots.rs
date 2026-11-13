// SPDX-License-Identifier: GPL-3.0-or-later
//! Preview fixtures from the supplied design. These tracks never enter the application store.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use eagle_ui::{
        Actions, CardItem, CollectionItem, DesktopWindow, Library, Player, PlayerColors, Route,
        Status, Storage, Theme, ThemeMode, TrackItem, Ui,
    };
    // Geometric placeholder artwork in the mockup's palette: background, disc, stripe.
    fn art(size: u32, back: [u8; 3], disc: [u8; 3], cx: f32, cy: f32) -> slint::Image {
        let mut buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(size, size);
        let edge = size as usize;
        for (i, pixel) in buffer.make_mut_slice().iter_mut().enumerate() {
            let (x, y) = (
                (i % edge) as f32 / size as f32,
                (i / edge) as f32 / size as f32,
            );
            let inside = (x - cx).powi(2) + (y - cy).powi(2) < 0.27f32.powi(2);
            let rgb = if inside || (y > 0.75 && y < 0.80) {
                disc
            } else {
                back
            };
            *pixel = slint::Rgba8Pixel::new(rgb[0], rgb[1], rgb[2], 255);
        }
        slint::Image::from_rgba8(buffer)
    }
    let palette: [([u8; 3], [u8; 3], f32, f32); 6] = [
        ([38, 70, 83], [233, 196, 106], 0.47, 0.51),
        ([61, 90, 128], [224, 251, 252], 0.47, 0.43),
        ([224, 122, 95], [244, 241, 222], 0.63, 0.53),
        ([129, 178, 154], [31, 58, 46], 0.35, 0.61),
        ([109, 89, 122], [234, 172, 139], 0.61, 0.39),
        ([242, 204, 143], [61, 64, 91], 0.68, 0.62),
    ];
    let album_of = |album: &str| match album {
        "Магистраль" => 0,
        "Полярная ночь" => 1,
        "Город спит" => 2,
        "Аналог" => 3,
        "Бетонный сад" => 4,
        _ => 5,
    };
    use slint::{ComponentHandle, VecModel};
    use std::{rc::Rc, time::Duration};
    let headless = std::env::args().any(|arg| arg == "--headless");
    if headless {
        use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
        struct PreviewPlatform(Rc<MinimalSoftwareWindow>);
        impl slint::platform::Platform for PreviewPlatform {
            fn create_window_adapter(
                &self,
            ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
                Ok(self.0.clone())
            }
        }
        slint::platform::set_platform(Box::new(PreviewPlatform(MinimalSoftwareWindow::new(
            RepaintBufferType::NewBuffer,
        ))))?;
    } else {
        eagle_ui::Renderer::from_args(std::env::args())?.select()?;
    }
    let ui = DesktopWindow::new()?;
    slint::select_bundled_translation("ru")?;
    ui.global::<Library>().set_playlists(
        Rc::new(VecModel::from(
            [
                "Любимые треки",
                "Поздний вечер",
                "Для дороги",
                "Фокус",
                "Скачанное",
            ]
            .iter()
            .enumerate()
            .map(|(i, title)| CollectionItem {
                id: format!("fixture:{i}").into(),
                title: (*title).into(),
            })
            .collect::<Vec<_>>(),
        ))
        .into(),
    );
    let sample = [
        ("Магистраль", "Шум волн", "Магистраль", "4:22"),
        ("Северный свет", "Мира Вольская", "Полярная ночь", "3:42"),
        ("Неон на Пресне", "Сигнал", "Город спит", "4:05"),
        ("Тёплый шум", "Ольга Корт", "Аналог", "3:18"),
        ("Бетонный сад", "Позднее лето", "Бетонный сад", "2:57"),
        ("До рассвета", "Тим Рязанов", "Окна", "3:51"),
        ("Пустые трамваи", "Сигнал", "Город спит", "3:34"),
        ("Медленная вода", "Ольга Корт", "Аналог", "4:47"),
        ("Последний поезд", "Мира Вольская", "Полярная ночь", "2:41"),
    ];
    let tracks = sample
        .iter()
        .enumerate()
        .map(|(i, (title, artist, album, duration))| TrackItem {
            occurrence: Default::default(),
            blocked_reason: Default::default(),
            id: format!("fixture:{i}").into(),
            album_id: format!("fixture:album:{i}").into(),
            album: (*album).into(),
            cover: {
                let (b, d, x, y) = palette[album_of(album)];
                art(64, b, d, x, y)
            },
            title: (*title).into(),
            artist: (*artist).into(),
            duration: (*duration).into(),
            available: true,
            liked: i == 1 || i == 4,
        })
        .collect::<Vec<_>>();
    ui.global::<Library>()
        .set_queue(Rc::new(VecModel::from(tracks[1..5].to_vec())).into());
    ui.global::<Library>()
        .set_tracks(Rc::new(VecModel::from(tracks.clone())).into());
    let albums = [
        ("Магистраль", "2024 · Шум волн"),
        ("Полярная ночь", "2023 · Мира Вольская"),
        ("Город спит", "2025 · Сигнал"),
        ("Аналог", "2022 · Ольга Корт"),
        ("Бетонный сад", "2024 · Позднее лето"),
        ("Окна", "2021 · Тим Рязанов"),
        ("Северный ветер", "2020 · Мира Вольская"),
        ("Тихий час", "2023 · Сигнал"),
    ];
    let album_cards = albums
        .iter()
        .enumerate()
        .map(|(i, (title, subtitle))| {
            let (b, d, x, y) = palette[i % palette.len()];
            CardItem {
                id: format!("fixture:album:{i}").into(),
                key: format!("a:{i}").into(),
                title: (*title).into(),
                subtitle: (*subtitle).into(),
                cover: art(300, b, d, x, y),
                round: false,
                route: Route::Album,
            }
        })
        .collect::<Vec<_>>();
    let artist_cards = [
        "Шум волн",
        "Мира Вольская",
        "Сигнал",
        "Ольга Корт",
        "Позднее лето",
        "Тим Рязанов",
    ]
    .iter()
    .enumerate()
    .map(|(i, name)| {
        let (b, d, x, y) = palette[(i + 2) % palette.len()];
        CardItem {
            id: format!("fixture:artist:{i}").into(),
            key: format!("r:{i}").into(),
            title: (*name).into(),
            subtitle: "".into(),
            cover: art(300, b, d, x, y),
            round: true,
            route: Route::Artist,
        }
    })
    .collect::<Vec<_>>();
    let artist_cover = art(300, [224, 122, 95], [244, 241, 222], 0.5, 0.5);
    let mut cover = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(600, 600);
    for (i, pixel) in cover.make_mut_slice().iter_mut().enumerate() {
        let (x, y) = ((i % 600) as f32 / 600., (i / 600) as f32 / 600.);
        let circle = (x - 0.47).powi(2) + (y - 0.51).powi(2) < 0.27f32.powi(2);
        let rgb = if circle || (y > 0.75 && y < 0.80) {
            [233, 196, 106]
        } else {
            [38, 70, 83]
        };
        *pixel = slint::Rgba8Pixel::new(rgb[0], rgb[1], rgb[2], 255);
    }
    let cover = slint::Image::from_rgba8(cover);
    let cover_image = cover.clone();
    ui.global::<Library>().set_cover(cover.clone());
    ui.global::<Player>().set_cover(cover);
    ui.global::<Library>().set_title("Поздний вечер".into());
    ui.global::<Library>().set_length(2000.);
    ui.global::<Ui>().set_route(Route::Playlist);
    ui.global::<Ui>().set_panel_open(true);
    ui.global::<Player>().set_track_id("fixture:0".into());
    ui.global::<Player>().set_title("Магистраль".into());
    ui.global::<Player>().set_artist("Шум волн".into());
    ui.global::<Player>().set_source("Яндекс Музыка".into());
    ui.global::<Player>().set_length(262.);
    ui.global::<Player>().set_quality("320 кбит/с".into());
    ui.global::<Player>().set_shown(84.);
    ui.global::<Player>().set_status(Status::Playing);
    ui.global::<Player>().set_volume(0.7);
    ui.global::<Player>().set_liked(false);
    ui.global::<Player>().on_position(|| 84.);
    ui.global::<Ui>().set_signed_in(true);
    let storage = ui.global::<Storage>();
    storage.set_cache_usage("4.0 МиБ".into());
    storage.set_saved_usage("8.0 МиБ".into());
    storage.set_staging_usage("1.0 МиБ".into());
    storage.set_other_usage("2.0 МиБ".into());
    storage.set_total_usage("15.0 МиБ".into());
    storage.set_folder("/data/eagle-preview".into());
    storage.set_network_hint("Лимит сети неизвестен; фоновые загрузки разрешены".into());
    ui.global::<Actions>()
        .on_valid_playlist_name(|name| !name.trim().is_empty());
    let folder = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/eagle-screenshots".into());
    std::fs::create_dir_all(&folder)?;
    // (file name, theme, route, panel open, drawer open, window width)
    let shots = [
        (
            "radio-empty-wide",
            ThemeMode::Graphite,
            Route::Radio,
            false,
            false,
            1280.,
        ),
        (
            "radio-empty-narrow",
            ThemeMode::Graphite,
            Route::Radio,
            false,
            false,
            760.,
        ),
        (
            "radio-filled-wide",
            ThemeMode::Graphite,
            Route::Radio,
            false,
            false,
            1280.,
        ),
        (
            "radio-filled-narrow",
            ThemeMode::Graphite,
            Route::Radio,
            false,
            false,
            760.,
        ),
        (
            "radio-error-narrow",
            ThemeMode::Graphite,
            Route::Radio,
            false,
            false,
            760.,
        ),
        (
            "playlist-edit-wide",
            ThemeMode::Graphite,
            Route::Playlist,
            false,
            false,
            1280.,
        ),
        (
            "playlist-edit-narrow",
            ThemeMode::Graphite,
            Route::Playlist,
            false,
            false,
            760.,
        ),
        (
            "playlist-conflict-narrow",
            ThemeMode::Graphite,
            Route::Playlist,
            false,
            false,
            760.,
        ),
        (
            "saved-empty-wide",
            ThemeMode::Graphite,
            Route::Saved,
            false,
            false,
            1280.,
        ),
        (
            "saved-empty-narrow",
            ThemeMode::Graphite,
            Route::Saved,
            false,
            false,
            760.,
        ),
        (
            "saved-blocked-wide",
            ThemeMode::Graphite,
            Route::Saved,
            false,
            false,
            1280.,
        ),
        (
            "saved-blocked-narrow",
            ThemeMode::Graphite,
            Route::Saved,
            false,
            false,
            760.,
        ),
        (
            "saved-error-wide",
            ThemeMode::Graphite,
            Route::Saved,
            false,
            false,
            1280.,
        ),
        (
            "saved-error-narrow",
            ThemeMode::Graphite,
            Route::Saved,
            false,
            false,
            760.,
        ),
        (
            "search-narrow",
            ThemeMode::Graphite,
            Route::Search,
            false,
            false,
            760.,
        ),
        (
            "create-playlist-narrow",
            ThemeMode::Graphite,
            Route::Library,
            false,
            false,
            760.,
        ),
        (
            "search-albums",
            ThemeMode::Graphite,
            Route::Search,
            false,
            false,
            1280.,
        ),
        (
            "create-playlist",
            ThemeMode::Graphite,
            Route::Library,
            false,
            false,
            1280.,
        ),
        (
            "create-playlist-paper",
            ThemeMode::Paper,
            Route::Library,
            false,
            false,
            1280.,
        ),
        (
            "delete-playlist",
            ThemeMode::Graphite,
            Route::Playlist,
            false,
            false,
            1280.,
        ),
        (
            "graphite",
            ThemeMode::Graphite,
            Route::Playlist,
            true,
            false,
            1280.,
        ),
        (
            "paper",
            ThemeMode::Paper,
            Route::Playlist,
            true,
            false,
            1280.,
        ),
        ("oled", ThemeMode::Oled, Route::Playlist, true, false, 1280.),
        (
            "paused-graphite",
            ThemeMode::Graphite,
            Route::Playlist,
            true,
            false,
            1280.,
        ),
        (
            "paused-paper",
            ThemeMode::Paper,
            Route::Playlist,
            true,
            false,
            1280.,
        ),
        (
            "paused-oled",
            ThemeMode::Oled,
            Route::Playlist,
            true,
            false,
            1280.,
        ),
        (
            "albums",
            ThemeMode::Graphite,
            Route::Albums,
            false,
            false,
            1280.,
        ),
        (
            "artists",
            ThemeMode::Paper,
            Route::Artists,
            false,
            false,
            1280.,
        ),
        (
            "artist",
            ThemeMode::Graphite,
            Route::Artist,
            false,
            false,
            1280.,
        ),
        (
            "settings-full",
            ThemeMode::Graphite,
            Route::Settings,
            false,
            false,
            1920.,
        ),
        (
            "settings-compact",
            ThemeMode::Paper,
            Route::Settings,
            false,
            false,
            780.,
        ),
        (
            "fullscreen",
            ThemeMode::Graphite,
            Route::Playlist,
            false,
            false,
            1920.,
        ),
        (
            "fullscreen-panel",
            ThemeMode::Paper,
            Route::Playlist,
            true,
            false,
            1920.,
        ),
        (
            "graphite-wide",
            ThemeMode::Graphite,
            Route::Playlist,
            false,
            false,
            1280.,
        ),
        (
            "settings",
            ThemeMode::Graphite,
            Route::Settings,
            false,
            false,
            1280.,
        ),
        (
            "settings-paper",
            ThemeMode::Paper,
            Route::Settings,
            false,
            false,
            1280.,
        ),
        (
            "narrow-panel",
            ThemeMode::Graphite,
            Route::Playlist,
            true,
            false,
            1000.,
        ),
        (
            "compact",
            ThemeMode::Paper,
            Route::Playlist,
            false,
            false,
            780.,
        ),
        (
            "compact-drawer",
            ThemeMode::Paper,
            Route::Playlist,
            false,
            true,
            780.,
        ),
        (
            "tight",
            ThemeMode::Graphite,
            Route::Playlist,
            false,
            false,
            600.,
        ),
    ];
    let index = Rc::new(std::cell::Cell::new(0));
    let capture_failures = Rc::new(std::cell::Cell::new(0));
    let progress = index.clone();
    let failures = capture_failures.clone();
    let timer = slint::Timer::default();
    let weak = ui.as_weak();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(900),
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let n = index.get();
            if n > 0 {
                let image = match ui.window().take_snapshot() {
                    Ok(image) => image,
                    Err(error) => {
                        eprintln!("snapshot {}: {error}", shots[n - 1].0);
                        capture_failures.set(capture_failures.get() + 1);
                        if capture_failures.get() >= 3 {
                            let _ = slint::quit_event_loop();
                        }
                        return;
                    }
                };
                capture_failures.set(0);
                let saved = image::save_buffer(
                    format!("{folder}/{}.png", shots[n - 1].0),
                    image.as_bytes(),
                    image.width(),
                    image.height(),
                    image::ColorType::Rgba8,
                );
                if let Err(error) = saved {
                    eprintln!("screenshot: {error}");
                    capture_failures.set(3);
                    let _ = slint::quit_event_loop();
                    return;
                }
            }
            if n == shots.len() {
                index.set(n + 1);
                let _ = slint::quit_event_loop();
                return;
            }
            let (name, mode, route, panel, drawer, width) = shots[n];
            let mut shown = tracks.clone();
            if name.starts_with("saved-empty") || name.starts_with("radio-empty") {
                shown.clear();
            }
            ui.global::<Player>().set_status(
                if name.starts_with("saved-blocked") || name.starts_with("paused-") {
                    Status::Paused
                } else {
                    Status::Playing
                },
            );
            if name.starts_with("saved-blocked") {
                shown[0].available = false;
                shown[0].blocked_reason = "Провайдер подтвердил потерю доступа".into();
            }
            ui.global::<Library>()
                .set_tracks(Rc::new(VecModel::from(shown)).into());
            ui.global::<Ui>().set_compact(name.starts_with("compact"));
            ui.global::<Ui>().set_radio_ready(
                name.starts_with("radio-filled") || name.starts_with("radio-error"),
            );
            ui.global::<Ui>().set_radio_busy(false);
            ui.global::<Ui>()
                .set_radio_notice(if name.starts_with("radio-error") {
                    "Нет сети. Очередь воспроизведения сохранена.".into()
                } else {
                    "".into()
                });
            ui.global::<Ui>()
                .set_error(if name.starts_with("playlist-conflict") {
                    "Плейлист изменился. Список обновлён; повторите действие.".into()
                } else if name.starts_with("saved-error") {
                    "Сеть недоступна. Сохранённая музыка остаётся на устройстве.".into()
                } else {
                    "".into()
                });

            ui.global::<Ui>()
                .set_create_playlist(name.starts_with("create-playlist"));
            ui.global::<Ui>()
                .set_playlist_name("Вечерняя прогулка".into());
            ui.global::<Ui>().set_search_query("Сигнал".into());
            ui.global::<Ui>().set_search_kind(2);
            ui.global::<Ui>()
                .set_confirm_delete(name == "delete-playlist");
            ui.global::<Library>()
                .set_deletable(name == "delete-playlist" || name.starts_with("playlist-"));
            ui.global::<Library>()
                .set_editable(name.starts_with("playlist-"));
            ui.global::<Library>().set_playlist_revision("3".into());
            ui.global::<Player>().set_album_id("yandex:10".into());
            ui.window().set_size(slint::LogicalSize::new(
                width,
                if width > 1600. { 1080. } else { 880. },
            ));
            ui.global::<Theme>().set_mode(mode);
            ui.global::<Ui>().set_route(route);
            let library = ui.global::<Library>();
            match route {
                Route::Radio => {
                    library.set_title("Моя волна".into());
                    library.set_subtitle("".into());
                }
                Route::Saved => {
                    library.set_title("Сохранённое".into());
                    library.set_cards(Rc::new(VecModel::<CardItem>::default()).into());
                }
                Route::Search => {
                    library.set_title("".into());
                    library.set_cards(Rc::new(VecModel::from(album_cards.clone())).into());
                }
                Route::Albums => {
                    library.set_title("".into());
                    library.set_cards(Rc::new(VecModel::from(album_cards.clone())).into());
                }
                Route::Artists => {
                    library.set_title("".into());
                    library.set_cards(Rc::new(VecModel::from(artist_cards.clone())).into());
                }
                Route::Artist => {
                    library.set_cards(Rc::new(VecModel::from(album_cards[2..6].to_vec())).into());
                    library.set_title("Сигнал".into());
                    library.set_cover(artist_cover.clone());
                    library.set_likeable(true);
                    library.set_liked(true);
                }
                _ => {
                    library.set_cards(Rc::new(VecModel::<CardItem>::default()).into());
                    library.set_title("Поздний вечер".into());
                    library.set_cover(cover_image.clone());
                    library.set_likeable(false);
                }
            }
            ui.global::<Ui>().set_panel_open(panel);
            ui.global::<Ui>().set_drawer_open(drawer);
            let theme = ui.global::<Theme>().get_t();
            let background = [theme.bg.red(), theme.bg.green(), theme.bg.blue()];
            let (bar, panel) =
                eagle_palette::player_surfaces([38, 70, 83], background, eagle_palette::Tint::Soft);
            let color = |c: [u8; 3]| slint::Color::from_rgb_u8(c[0], c[1], c[2]);
            ui.global::<Player>().set_colors(PlayerColors {
                bg: color(panel.background),
                bar: color(bar.background),
                tx: color(panel.text),
                tx2: color(panel.secondary),
            });
            index.set(n + 1);
        },
    );
    if headless {
        ui.show()?;
        let started = std::time::Instant::now();
        while progress.get() <= shots.len()
            && failures.get() < 3
            && started.elapsed() < Duration::from_secs(120)
        {
            slint::platform::update_timers_and_animations();
            std::thread::sleep(Duration::from_millis(5));
        }
        if progress.get() <= shots.len() {
            return Err("preview did not capture every state".into());
        }
    } else {
        ui.run()?;
    }
    if failures.get() >= 3 {
        return Err("preview capture failed".into());
    }
    Ok(())
}
#[cfg(any(target_os = "android", target_os = "ios"))]
fn main() {}
