// SPDX-License-Identifier: GPL-3.0-or-later
//! Domain events are applied only on the Slint event-loop thread.
use crate::{
    Actions, CollectionItem, DesktopWindow, Library, Player, PlayerColors, Route, Status, Theme,
    TrackItem, Ui, media,
};
use eagle_app::session::{Command, Event, LibraryDelta, LibraryItem};
use eagle_engine::{PositionAnchor, State};
use slint::{ComponentHandle, Model, VecModel};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
fn track_item(item: LibraryItem) -> TrackItem {
    let seconds = item.track.duration_ms / 1000;
    TrackItem {
        id: item.track.id.to_string().into(),
        album_id: item
            .track
            .album
            .as_ref()
            .map_or_else(String::new, ToString::to_string)
            .into(),
        title: item.track.title.as_str().into(),
        artist: item
            .track
            .artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
            .into(),
        duration: format!("{}:{:02}", seconds / 60, seconds % 60).into(),
        liked: item.liked,
        available: item.track.availability == eagle_app::session::Availability::Available,
    }
}
fn apply(
    ui: &DesktopWindow,
    event: Event,
    rows: &VecModel<TrackItem>,
    anchor: &Mutex<Option<PositionAnchor>>,
) {
    match event {
        Event::Collections(collections) => {
            let model = ui.global::<Library>().get_playlists();
            let Some(model) = model.as_any().downcast_ref::<VecModel<CollectionItem>>() else {
                return;
            };
            for (index, collection) in collections.iter().enumerate() {
                let item = CollectionItem {
                    id: collection.id.to_string().into(),
                    title: collection.title.as_str().into(),
                };
                if index < model.row_count() {
                    if model.row_data(index) != Some(item.clone()) {
                        model.set_row_data(index, item);
                    }
                } else {
                    model.push(item);
                }
            }
            while model.row_count() > collections.len() {
                model.remove(model.row_count() - 1);
            }
        }
        Event::Collection { title, length } => {
            ui.global::<Library>().set_title(title.into());
            ui.global::<Library>().set_length(length.as_secs_f32());
        }
        Event::Queue(tracks) => {
            let items = tracks
                .into_iter()
                .map(|track| {
                    track_item(LibraryItem {
                        track,
                        liked: false,
                    })
                })
                .collect::<Vec<_>>();
            ui.global::<Library>()
                .set_queue(Rc::new(VecModel::from(items)).into());
        }
        Event::View(view) => {
            ui.global::<Library>().set_title("".into());
            ui.global::<Library>().set_cover(slint::Image::default());
            ui.global::<Ui>().set_route(match view {
                eagle_app::session::View::Library => Route::Library,
                eagle_app::session::View::Search => Route::Search,
                eagle_app::session::View::Album => Route::Album,
                eagle_app::session::View::Playlist => Route::Playlist,
                eagle_app::session::View::Login => Route::Login,
                eagle_app::session::View::Settings => Route::Settings,
            });
            ui.global::<Ui>().set_error("".into());
        }
        Event::Busy(busy) => ui.global::<Ui>().set_busy(busy),
        Event::Player(state) => {
            media::state(&state);
            let player = ui.global::<Player>();
            player.set_status(match state.status {
                State::Idle => Status::Idle,
                State::Loading => Status::Loading,
                State::Playing => Status::Playing,
                State::Paused => Status::Paused,
                State::Buffering => Status::Buffering,
            });
            player.set_volume(state.volume);
            player.set_liked(state.liked);
            player.set_shuffle(state.shuffle);
            player.set_repeat(match state.repeat {
                eagle_app::session::Repeat::Off => 0,
                eagle_app::session::Repeat::All => 1,
                eagle_app::session::Repeat::One => 2,
            });
            if let Some(track) = &state.track {
                if player.get_track_id().as_str() != track.id.to_string() {
                    *anchor.lock().unwrap_or_else(|p| p.into_inner()) = None;
                    player.set_shown(0.);
                    player.set_cover(slint::Image::default());
                    COVER_COLOR.with(|c| *c.borrow_mut() = [128; 3]);
                    apply_colors(ui);
                }
                player.set_track_id(track.id.to_string().into());
                player.set_title(track.title.as_str().into());
                player.set_source(track.id.provider().into());
                player.set_artist(
                    track
                        .artists
                        .iter()
                        .map(|a| a.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                        .into(),
                );
                player.set_length(track.duration_ms as f32 / 1000.);
            }
        }
        Event::Position(position) => {
            media::position(&position);
            ui.global::<Player>().set_shown(
                eagle_app::session::position_now(&position, Instant::now()).as_secs_f32(),
            );
            *anchor.lock().unwrap_or_else(|p| p.into_inner()) = Some(position);
        }
        Event::Library(delta) => {
            for change in delta {
                match change {
                    LibraryDelta::Insert { index, item } => rows.insert(index, track_item(item)),
                    LibraryDelta::Update { index, item } => {
                        rows.set_row_data(index, track_item(item))
                    }
                    LibraryDelta::Remove { index } => {
                        rows.remove(index);
                    }
                }
            }
        }
        Event::Error(message) => ui.global::<Ui>().set_error(message.into()),
        Event::Cover(target, id, cover) => {
            if matches!(target, eagle_app::session::CoverTarget::Collection) {
                let pixels = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                    &cover.rgba,
                    cover.width,
                    cover.height,
                );
                ui.global::<Library>()
                    .set_cover(slint::Image::from_rgba8(pixels));
                return;
            }
            if ui.global::<Player>().get_track_id().as_str() == id.to_string() {
                let pixels = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                    &cover.rgba,
                    cover.width,
                    cover.height,
                );
                ui.global::<Player>()
                    .set_cover(slint::Image::from_rgba8(pixels));
                COVER_COLOR.with(|c| *c.borrow_mut() = cover.dominant);
                apply_colors(ui);
            }
        }
        Event::Account => ui.global::<Ui>().set_signed_in(true),
    }
}

thread_local! { static COVER_COLOR: RefCell<[u8; 3]> = const { RefCell::new([128; 3]) }; }
pub(super) fn apply_colors(ui: &DesktopWindow) {
    let tokens = ui.global::<Theme>().get_t();
    let background = [tokens.bg.red(), tokens.bg.green(), tokens.bg.blue()];
    let tint = match ui.global::<Theme>().get_tint() {
        0 => eagle_palette::Tint::Off,
        2 => eagle_palette::Tint::Bright,
        _ => eagle_palette::Tint::Soft,
    };
    let (bar, panel) =
        COVER_COLOR.with(|c| eagle_palette::player_surfaces(*c.borrow(), background, tint));
    let color = |rgb: [u8; 3]| slint::Color::from_rgb_u8(rgb[0], rgb[1], rgb[2]);
    ui.global::<Player>().set_colors(PlayerColors {
        bg: color(panel.background),
        bar: color(bar.background),
        tx: color(panel.text),
        tx2: color(panel.secondary),
    });
}

type BridgeState = (Rc<VecModel<TrackItem>>, Arc<Mutex<Option<PositionAnchor>>>);
thread_local! { static UI_BRIDGE: RefCell<Option<BridgeState>> = const { RefCell::new(None) }; }
pub(super) fn bind(ui: &DesktopWindow, sender: eagle_app::session::CommandSender) {
    let actions = ui.global::<Actions>();
    let tx = sender.clone();
    actions.on_play_all(move |shuffle| {
        let _ = tx.send(Command::PlayAll(shuffle));
    });
    macro_rules! simple {
        ($method:ident, $command:expr) => {{
            let tx = sender.clone();
            actions.$method(move || {
                let _ = tx.send($command);
            });
        }};
    }
    simple!(on_toggle_play, Command::TogglePlay);
    simple!(on_next, Command::Next);
    simple!(on_toggle_shuffle, Command::ToggleShuffle);
    simple!(on_cycle_repeat, Command::CycleRepeat);
    simple!(on_prev, Command::Previous);
    let tx = sender.clone();
    actions.on_play(move |id| {
        if let Ok(id) = id.parse() {
            let _ = tx.send(Command::Play(id));
        }
    });
    let tx = sender.clone();
    actions.on_like(move |id| {
        if let Ok(id) = id.parse() {
            let _ = tx.send(Command::Like(id));
        }
    });
    let tx = sender.clone();
    actions.on_seek(move |seconds| {
        if seconds.is_finite() && seconds >= 0. {
            let _ = tx.send(Command::Seek(Duration::from_secs_f32(seconds)));
        }
    });
    let tx = sender.clone();
    actions.on_set_volume(move |v| {
        let _ = tx.send(Command::Volume(v));
    });
    let tx = sender.clone();
    actions.on_search(move |text| {
        let _ = tx.send(Command::Search(text.to_string()));
    });
    let tx = sender.clone();
    actions.on_login(move |token| {
        let _ = tx.send(Command::Login(eagle_app::session::Token::new(
            token.to_string(),
        )));
    });
    let tx = sender.clone();
    actions.on_collection(move |route, id| {
        if let Ok(id) = id.parse() {
            let _ = tx.send(if route == Route::Album {
                Command::Album(id)
            } else {
                Command::Playlist(id)
            });
        }
    });
    actions.on_open(move |route| {
        use eagle_app::session::View;
        let view = match route {
            Route::Library => View::Library,
            Route::Search => View::Search,
            Route::Album => View::Album,
            Route::Playlist => View::Playlist,
            Route::Login => View::Login,
            Route::Settings => View::Settings,
        };
        let _ = sender.send(Command::Open(view));
    });
}

pub(super) fn initialize(
    rows: Rc<VecModel<TrackItem>>,
    anchor: Arc<Mutex<Option<PositionAnchor>>>,
) {
    UI_BRIDGE.with(|bridge| *bridge.borrow_mut() = Some((rows, anchor)));
}
pub(super) fn deliver(ui: &DesktopWindow, event: Event) {
    UI_BRIDGE.with(|bridge| {
        if let Some((rows, anchor)) = bridge.borrow().as_ref() {
            apply(ui, event, rows, anchor);
        }
    });
}
pub(super) fn clear() {
    UI_BRIDGE.with(|bridge| bridge.borrow_mut().take());
}

/// Hidden windows retain the newest anchor without posting a Slint event.
pub(super) fn coalesce_hidden_position(
    event: Event,
    visible: bool,
    anchor: &Mutex<Option<PositionAnchor>>,
) -> Option<Event> {
    match event {
        Event::Position(position) if !visible => {
            *anchor.lock().unwrap_or_else(|p| p.into_inner()) = Some(position);
            None
        }
        event => Some(event),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hidden_positions_update_anchor_without_scheduling_ui_work() {
        let anchor = Mutex::new(None);
        let position = PositionAnchor {
            track: 0,
            position: Duration::from_secs(4),
            audible_at: Instant::now(),
            state: State::Playing,
        };
        assert!(
            coalesce_hidden_position(Event::Position(position.clone()), false, &anchor).is_none()
        );
        assert_eq!(
            anchor.lock().unwrap().as_ref().unwrap().position,
            position.position
        );
        assert!(matches!(
            coalesce_hidden_position(Event::Position(position), true, &anchor),
            Some(Event::Position(_))
        ));
    }
}
