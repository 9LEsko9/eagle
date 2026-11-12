// SPDX-License-Identifier: GPL-3.0-or-later
//! Preview fixtures from the supplied design. These tracks never enter the application store.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use eagle_ui::{
        CollectionItem, DesktopWindow, Library, Player, PlayerColors, Route, Status, Theme,
        ThemeMode, TrackItem, Ui,
    };
    use slint::{ComponentHandle, VecModel};
    use std::{rc::Rc, time::Duration};
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .select()?;
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
        ("Магистраль", "Шум волн", "4:22"),
        ("Северный свет", "Мира Вольская", "3:42"),
        ("Неон на Пресне", "Сигнал", "4:05"),
        ("Тёплый шум", "Ольга Корт", "3:18"),
        ("Бетонный сад", "Позднее лето", "2:57"),
        ("До рассвета", "Тим Рязанов", "3:51"),
        ("Пустые трамваи", "Сигнал", "3:34"),
        ("Медленная вода", "Ольга Корт", "4:47"),
        ("Последний поезд", "Мира Вольская", "2:41"),
    ];
    let tracks = sample
        .iter()
        .enumerate()
        .map(|(i, (title, artist, duration))| TrackItem {
            id: format!("fixture:{i}").into(),
            album_id: "".into(),
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
        .set_tracks(Rc::new(VecModel::from(tracks)).into());
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
    ui.global::<Library>().set_cover(cover.clone());
    ui.global::<Player>().set_cover(cover);
    ui.global::<Library>().set_title("Поздний вечер".into());
    ui.global::<Library>().set_length(2000.);
    ui.global::<Ui>().set_route(Route::Playlist);
    ui.global::<Ui>().set_panel_open(true);
    ui.global::<Player>().set_track_id("fixture:0".into());
    ui.global::<Player>().set_title("Магистраль".into());
    ui.global::<Player>().set_artist("Шум волн".into());
    ui.global::<Player>()
        .set_source("Данные предпросмотра".into());
    ui.global::<Player>().set_length(262.);
    ui.global::<Player>().set_shown(84.);
    ui.global::<Player>().set_status(Status::Paused);
    let folder = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/eagle-screenshots".into());
    std::fs::create_dir_all(&folder)?;
    let index = Rc::new(std::cell::Cell::new(0));
    let timer = slint::Timer::default();
    let weak = ui.as_weak();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(800),
        move || {
            let ui = weak.upgrade().unwrap();
            let n = index.get();
            if n > 0 {
                let image = ui.window().take_snapshot().unwrap();
                image::save_buffer(
                    format!("{folder}/{}.png", ["graphite", "paper", "oled"][n - 1]),
                    image.as_bytes(),
                    image.width(),
                    image.height(),
                    image::ColorType::Rgba8,
                )
                .unwrap();
            }
            if n == 3 {
                slint::quit_event_loop().unwrap();
                return;
            }
            ui.global::<Theme>()
                .set_mode([ThemeMode::Graphite, ThemeMode::Paper, ThemeMode::Oled][n]);
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
    ui.run()?;
    Ok(())
}
#[cfg(any(target_os = "android", target_os = "ios"))]
fn main() {}
