// SPDX-License-Identifier: GPL-3.0-or-later
//! Domain events are applied only on the Slint event-loop thread.
use crate::{
    Actions, CardItem, CollectionItem, DesktopWindow, DownloadItem, Library, OfferPhase, Player,
    PlayerColors, ProviderItem, QueueItem, Route, SignInKind, SignInMethodItem, SignInPhase,
    Status, Storage, Theme, TrackItem, Ui, Wave, WaveColors, WaveTrack,
};
use eagle_app::providers::ProviderStatus;
use eagle_app::session::{
    Card, Command, CommandSender, DownloadState, Event, Header, LibraryDelta, LibraryItem, Quality,
    SignInState, View, WaveMark, WaveState,
};
use eagle_engine::{PositionAnchor, State};
use slint::{ComponentHandle, Model, VecModel};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    ops::Range,
    rc::Rc,
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};

/// Rows kept with artwork around the visible range; everything else is released.
const MARGIN: usize = 8;

fn image(cover: &eagle_palette::Cover) -> slint::Image {
    slint::Image::from_rgba8(
        slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
            &cover.rgba,
            cover.width,
            cover.height,
        ),
    )
}
fn track_item(item: LibraryItem) -> TrackItem {
    let seconds = item.track.duration_ms / 1000;
    TrackItem {
        occurrence: Default::default(),
        blocked_reason: item.blocked_reason.clone().unwrap_or_default().into(),
        id: item.track.id.to_string().into(),
        album_id: item
            .track
            .album
            .as_ref()
            .map_or_else(String::new, ToString::to_string)
            .into(),
        album: item.track.album_title.as_deref().unwrap_or_default().into(),
        cover: slint::Image::default(),
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
        available: item.blocked_reason.is_none()
            && item.track.availability == eagle_app::session::Availability::Available,
    }
}
fn artists_line(track: &eagle_core::Track) -> String {
    track
        .artists
        .iter()
        .map(|a| a.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}
fn wave_item(track: &eagle_core::Track, mark: WaveMark, cover: slint::Image) -> WaveTrack {
    WaveTrack {
        id: track.id.to_string().into(),
        title: track.title.as_str().into(),
        artist: artists_line(track).into(),
        cover,
        mark: match mark {
            WaveMark::None => "",
            WaveMark::Skipped => "не то",
            WaveMark::Liked => "♥",
        }
        .into(),
    }
}
thread_local! { static TUNING_SYNCED: Cell<bool> = const { Cell::new(false) }; }
/// Shows the wave feed. Covers that are already loaded stay; the others are asked for.
/// The tuning is taken over once per bridge: afterwards the screen owns it.
fn apply_wave(ui: &DesktopWindow, bridge: &Bridge, state: WaveState) {
    let wave = ui.global::<Wave>();
    if !TUNING_SYNCED.with(|synced| synced.replace(true)) && !wave.get_dragging() {
        let (x, y) = state.tuning.mood.unwrap_or((0., 0.));
        wave.set_mood_x(x);
        wave.set_mood_y(y);
        wave.set_novelty(state.tuning.novelty);
    }
    let mut known: HashMap<String, slint::Image> = wave
        .get_played()
        .iter()
        .chain(std::iter::once(wave.get_next()))
        .filter(|item| item.cover.size().width > 0)
        .map(|item| (item.id.to_string(), item.cover))
        .collect();
    let mut take =
        |track: &eagle_core::Track| known.remove(&track.id.to_string()).unwrap_or_default();
    let played: Vec<WaveTrack> = state
        .previous
        .iter()
        .map(|item| wave_item(&item.track, item.mark, take(&item.track)))
        .collect();
    let next = state
        .next
        .as_ref()
        .map(|track| wave_item(track, WaveMark::None, take(track)));
    let missing: Vec<String> = played
        .iter()
        .chain(next.iter())
        .filter(|item| item.cover.size().width == 0)
        .map(|item| format!("t:{}", item.id))
        .collect();
    wave.set_active(state.current.is_some());
    wave.set_played(Rc::new(VecModel::from(played)).into());
    wave.set_has_next(next.is_some());
    wave.set_next(next.unwrap_or_default());
    wave.set_why(state.why.into());
    wave.set_note(state.note.into());
    if !missing.is_empty() && ui.global::<Ui>().get_list_covers() {
        let _ = bridge.sender.send(Command::Thumbnails(missing));
    }
}
fn card_item(card: Card) -> CardItem {
    let route = if card.key.starts_with("p:") {
        Route::Playlist
    } else if card.key.starts_with("r:") {
        Route::Artist
    } else {
        Route::Album
    };
    CardItem {
        id: card.id.to_string().into(),
        key: card.key.into(),
        title: card.title.into(),
        subtitle: card.subtitle.into(),
        cover: slint::Image::default(),
        round: card.round,
        route,
    }
}
fn route(view: View) -> Route {
    match view {
        View::Saved => Route::Saved,
        View::History => Route::History,
        View::Library => Route::Library,
        View::Search => Route::Search,
        View::Album => Route::Album,
        View::Playlist => Route::Playlist,
        View::Settings => Route::Settings,
        View::Albums => Route::Albums,
        View::Artists => Route::Artists,
        View::Artist => Route::Artist,
        View::Radio => Route::Radio,
    }
}

/// The user's answer about sending local likes to a newly connected service.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Offer {
    /// This many likes could be sent; the user has not answered.
    Asking(usize),
    Sent,
    Skipped,
}

/// «1 лайк», «2 лайка», «5 лайков».
fn likes_phrase(count: usize) -> String {
    let word = match (count % 100, count % 10) {
        (11..=14, _) => "лайков",
        (_, 1) => "лайк",
        (_, 2..=4) => "лайка",
        _ => "лайков",
    };
    format!("{count} {word}")
}

/// Everything the services section shows, kept so a change to one part rebuilds its row.
#[derive(Default)]
struct Services {
    statuses: RefCell<Vec<ProviderStatus>>,
    sign_in: RefCell<HashMap<String, SignInState>>,
    offers: RefCell<HashMap<String, Offer>>,
    /// Rows are updated in place, so a field being typed in survives every event.
    rows: Rc<VecModel<ProviderItem>>,
    attached: Cell<bool>,
}

/// A link without the scheme and the closing slash: `https://ya.ru/device` becomes `ya.ru/device`.
fn link_label(url: &str) -> String {
    let bare = url.split_once("://").map_or(url, |(_, rest)| rest);
    bare.trim_end_matches('/').to_owned()
}

fn provider_item(
    status: &ProviderStatus,
    state: Option<&SignInState>,
    offer: Option<Offer>,
) -> ProviderItem {
    let methods = status
        .sign_in
        .iter()
        .filter_map(|method| {
            use eagle_core::SignInMethod;
            Some(match method {
                SignInMethod::DeviceCode => SignInMethodItem {
                    kind: SignInKind::DeviceCode,
                    hint: Default::default(),
                    help_url: Default::default(),
                },
                SignInMethod::Token { hint, help_url } => SignInMethodItem {
                    kind: SignInKind::Token,
                    hint: (*hint).into(),
                    help_url: help_url.unwrap_or_default().into(),
                },
                SignInMethod::Password { login_hint } => SignInMethodItem {
                    kind: SignInKind::Password,
                    hint: (*login_hint).into(),
                    help_url: Default::default(),
                },
                SignInMethod::Browser => SignInMethodItem {
                    kind: SignInKind::Browser,
                    hint: Default::default(),
                    help_url: Default::default(),
                },
                // A method this build does not know is not offered.
                _ => return None,
            })
        })
        .collect::<Vec<_>>();
    let mut item = ProviderItem {
        id: status.id.as_str().into(),
        name: status.name.as_str().into(),
        signed_in: status.signed_in,
        account: status.account_name.clone().unwrap_or_default().into(),
        methods: Rc::new(VecModel::from(methods)).into(),
        phase: SignInPhase::Idle,
        offer: OfferPhase::None,
        ..Default::default()
    };
    match state {
        None | Some(SignInState::Idle) => {}
        Some(SignInState::Busy) => item.phase = SignInPhase::Busy,
        Some(SignInState::Code { code, url, expired }) => {
            item.phase = SignInPhase::Code;
            item.code = code.as_str().into();
            item.link = url.as_str().into();
            item.link_label = link_label(url).into();
            item.expired = *expired;
        }
        Some(SignInState::Browser { url }) => {
            item.phase = SignInPhase::Browser;
            item.link = url.as_str().into();
        }
        Some(SignInState::SecondFactor { hint }) => {
            item.phase = SignInPhase::SecondFactor;
            item.detail = hint.as_str().into();
        }
        Some(SignInState::Failed(text)) => {
            item.phase = SignInPhase::Failed;
            item.detail = text.as_str().into();
        }
    }
    if status.signed_in {
        match offer {
            Some(Offer::Asking(count)) => {
                item.offer = OfferPhase::Asking;
                item.offer_text =
                    format!("Отправить {} из eagle в этот сервис?", likes_phrase(count)).into();
            }
            Some(Offer::Sent) => item.offer = OfferPhase::Sent,
            Some(Offer::Skipped) => item.offer = OfferPhase::Skipped,
            None => {}
        }
    }
    item
}

impl Services {
    /// Shows the current statuses, sign-in states and answers.
    fn publish(&self, ui: &DesktopWindow) {
        if !self.attached.replace(true) {
            ui.global::<Ui>().set_providers(self.rows.clone().into());
        }
        let statuses = self.statuses.borrow();
        let states = self.sign_in.borrow();
        let offers = self.offers.borrow();
        let items = statuses
            .iter()
            .map(|status| {
                provider_item(
                    status,
                    states.get(&status.id),
                    offers.get(&status.id).copied(),
                )
            })
            .collect::<Vec<_>>();
        if self.rows.row_count() == items.len() {
            for (index, item) in items.into_iter().enumerate() {
                self.rows.set_row_data(index, item);
            }
        } else {
            self.rows.set_vec(items);
        }
    }
}

/// UI-thread state shared by event delivery and actions.
struct Bridge {
    services: Services,
    rows: Rc<VecModel<TrackItem>>,
    search_rows: RefCell<Vec<LibraryItem>>,
    search_cards: RefCell<Vec<Card>>,
    search_id: Cell<u64>,
    cards: Rc<VecModel<CardItem>>,
    anchor: Arc<Mutex<Option<PositionAnchor>>>,
    sender: CommandSender,
    /// Visible ranges reported by the lists, as (first, count).
    row_window: Cell<(usize, usize)>,
    card_window: Cell<(usize, usize)>,
    images: RefCell<HashMap<usize, (Weak<eagle_palette::Cover>, slint::Image)>>,
    queue_ids: RefCell<Vec<u64>>,
    queue_current: Cell<Option<u64>>,
}
impl Bridge {
    fn filter_search(&self, ui: &DesktopWindow) {
        let provider = ui.global::<Ui>().get_search_provider();
        self.clear_rows();
        self.rows.set_vec(
            self.search_rows
                .borrow()
                .iter()
                .filter(|item| {
                    provider.is_empty()
                        || item.track.id.provider() == provider.as_str()
                        || item
                            .sources
                            .iter()
                            .any(|id| id.provider() == provider.as_str())
                })
                .cloned()
                .map(track_item)
                .collect::<Vec<_>>(),
        );
        self.refresh_rows(ui);
        self.cards.set_vec(
            self.search_cards
                .borrow()
                .iter()
                .filter(|card| {
                    provider.is_empty()
                        || card.id.provider() == provider.as_str()
                        || card
                            .sources
                            .iter()
                            .any(|id| id.provider() == provider.as_str())
                })
                .cloned()
                .map(card_item)
                .collect::<Vec<_>>(),
        );
        let (first, count) = self.card_window.get();
        self.update_cards(first, count, true);
    }
    fn image(&self, cover: &Arc<eagle_palette::Cover>) -> slint::Image {
        let key = Arc::as_ptr(cover) as usize;
        let mut images = self.images.borrow_mut();
        if let Some((source, image)) = images.get(&key)
            && source
                .upgrade()
                .is_some_and(|source| Arc::ptr_eq(&source, cover))
        {
            return image.clone();
        }
        let converted = image(cover);
        images.insert(key, (Arc::downgrade(cover), converted.clone()));
        converted
    }
    fn release_unused_images(&self, ui: &DesktopWindow) {
        let mut active = vec![
            ui.global::<Library>().get_cover(),
            ui.global::<Player>().get_cover(),
        ];
        active.extend(
            Self::padded(self.row_window.get(), self.rows.row_count())
                .filter_map(|index| self.rows.row_data(index))
                .map(|row| row.cover),
        );
        active.extend(
            self.card_range(self.card_window.get())
                .filter_map(|index| self.cards.row_data(index))
                .map(|card| card.cover),
        );
        active.extend(
            ui.global::<Library>()
                .get_queue()
                .iter()
                .map(|row| row.cover),
        );
        let wave = ui.global::<Wave>();
        active.extend(wave.get_played().iter().map(|row| row.cover));
        active.push(wave.get_next().cover);
        self.images
            .borrow_mut()
            .retain(|_, (_, image)| active.contains(image));
    }
    fn card_range(&self, window: (usize, usize)) -> Range<usize> {
        // The grid reports its one-row overscan; do not add track-row margins here.
        window.0.min(self.cards.row_count())
            ..window
                .0
                .saturating_add(window.1)
                .min(self.cards.row_count())
    }
    fn padded(window: (usize, usize), len: usize) -> Range<usize> {
        let start = window.0.saturating_sub(MARGIN).min(len);
        let end = (window.0 + window.1 + MARGIN).min(len);
        start..end
    }
    /// Releases artwork outside the new window and asks for what is missing inside it.
    fn rows_visible(&self, ui: &DesktopWindow, first: usize, count: usize) {
        self.update_rows(ui, first, count, false);
    }
    fn update_rows(&self, ui: &DesktopWindow, first: usize, count: usize, force: bool) {
        if !force && self.row_window.get() == (first, count) {
            return;
        }
        let old = Self::padded(
            self.row_window.replace((first, count)),
            self.rows.row_count(),
        );
        let new = Self::padded((first, count), self.rows.row_count());
        for index in old.filter(|i| !new.contains(i)) {
            if let Some(mut row) = self.rows.row_data(index)
                && row.cover.size().width > 0
            {
                row.cover = slint::Image::default();
                self.rows.set_row_data(index, row);
            }
        }
        if !ui.global::<Ui>().get_list_covers() {
            return;
        }
        let keys = new
            .filter_map(|i| self.rows.row_data(i))
            .filter(|row| row.cover.size().width == 0)
            .map(|row| format!("t:{}", row.id))
            .collect::<Vec<_>>();
        if !keys.is_empty() {
            let _ = self.sender.send(Command::Thumbnails(keys));
        }
    }
    fn cards_visible(&self, first: usize, count: usize) {
        self.update_cards(first, count, false);
    }
    fn update_cards(&self, first: usize, count: usize, force: bool) {
        if !force && self.card_window.get() == (first, count) {
            return;
        }
        let old = self.card_range(self.card_window.replace((first, count)));
        let new = self.card_range((first, count));
        for index in old.filter(|i| !new.contains(i)) {
            if let Some(mut card) = self.cards.row_data(index)
                && card.cover.size().width > 0
            {
                card.cover = slint::Image::default();
                self.cards.set_row_data(index, card);
            }
        }
        let keys = new
            .filter_map(|i| self.cards.row_data(i))
            .filter(|card| card.cover.size().width == 0)
            .map(|card| card.key.to_string())
            .collect::<Vec<_>>();
        if !keys.is_empty() {
            let _ = self.sender.send(Command::Thumbnails(keys));
        }
    }
    fn thumbnail(&self, ui: &DesktopWindow, key: &str, cover: &Arc<eagle_palette::Cover>) {
        if let Some(id) = key.strip_prefix("h:") {
            if ui.global::<Library>().get_header_id().as_str() == id {
                ui.global::<Library>().set_cover(self.image(cover));
            }
        } else if let Some(id) = key.strip_prefix("t:") {
            if !ui.global::<Ui>().get_list_covers() {
                return;
            }
            let mut converted = None;
            let range = Self::padded(self.row_window.get(), self.rows.row_count());
            for index in range {
                if let Some(mut row) = self.rows.row_data(index)
                    && row.id.as_str() == id
                {
                    row.cover = converted.get_or_insert_with(|| self.image(cover)).clone();
                    self.rows.set_row_data(index, row);
                }
            }
            let wave = ui.global::<Wave>();
            let wave_rows = wave.get_played();
            if let Some(model) = wave_rows.as_any().downcast_ref::<VecModel<WaveTrack>>() {
                for index in 0..model.row_count() {
                    if let Some(mut row) = model.row_data(index)
                        && row.id.as_str() == id
                    {
                        row.cover = converted.get_or_insert_with(|| self.image(cover)).clone();
                        model.set_row_data(index, row);
                    }
                }
            }
            let mut next = wave.get_next();
            if next.id.as_str() == id {
                next.cover = converted.get_or_insert_with(|| self.image(cover)).clone();
                wave.set_next(next);
            }
            let queue = ui.global::<Library>().get_queue();
            for index in 0..if ui.global::<Ui>().get_panel_open() {
                queue.row_count()
            } else {
                0
            } {
                if let Some(mut row) = queue.row_data(index)
                    && row.id.as_str() == id
                {
                    row.cover = converted.get_or_insert_with(|| self.image(cover)).clone();
                    queue.set_row_data(index, row);
                }
            }
        } else {
            let mut converted = None;
            let range = self.card_range(self.card_window.get());
            for index in range {
                if let Some(mut card) = self.cards.row_data(index)
                    && card.key.as_str() == key
                {
                    card.cover = converted.get_or_insert_with(|| self.image(cover)).clone();
                    self.cards.set_row_data(index, card);
                }
            }
        }
    }
    fn clear_rows(&self) {
        for index in Self::padded(self.row_window.get(), self.rows.row_count()) {
            if let Some(mut row) = self.rows.row_data(index)
                && row.cover.size().width > 0
            {
                row.cover = slint::Image::default();
                self.rows.set_row_data(index, row);
            }
        }
    }
    fn list_covers_changed(&self, ui: &DesktopWindow) {
        let queue = ui.global::<Library>().get_queue();
        if !ui.global::<Ui>().get_list_covers() {
            self.clear_rows();
            for index in 0..queue.row_count() {
                if let Some(mut row) = queue.row_data(index) {
                    row.cover = slint::Image::default();
                    queue.set_row_data(index, row);
                }
            }
        } else {
            self.refresh_rows(ui);
            let keys = (0..queue.row_count())
                .filter_map(|index| queue.row_data(index))
                .filter(|row| row.cover.size().width == 0)
                .map(|row| format!("t:{}", row.id))
                .collect::<Vec<_>>();
            if !keys.is_empty() {
                let _ = self.sender.send(Command::Thumbnails(keys));
            }
        }
    }
    /// Re-requests artwork after the list content changed under the window.
    fn refresh_rows(&self, ui: &DesktopWindow) {
        let (first, count) = self.row_window.get();
        self.update_rows(ui, first, count, true);
    }
}

fn apply(ui: &DesktopWindow, event: Event, bridge: &Bridge) {
    let rows = &bridge.rows;
    let anchor = &bridge.anchor;
    match event {
        Event::Snapshot(snapshot) => {
            let playlist_busy = snapshot.playlist_mutation_busy;
            let playlist_notice = snapshot.playlist_mutation_notice.clone();
            let radio_ready = snapshot.radio_ready;
            let radio_busy = snapshot.radio_busy;
            let radio_notice = snapshot.radio_notice.clone();
            let wave = snapshot.wave.clone();
            ui.global::<Ui>().set_signed_in(snapshot.signed_in);
            apply(ui, Event::View(snapshot.view), bridge);
            apply(ui, Event::Collections(snapshot.collections), bridge);
            apply(
                ui,
                Event::Collection {
                    title: snapshot.title,
                    length: snapshot.length,
                },
                bridge,
            );
            apply(ui, Event::Header(snapshot.header), bridge);
            apply(ui, Event::LibraryReset(snapshot.rows), bridge);
            apply(ui, Event::Cards(snapshot.cards), bridge);
            apply(ui, Event::Busy(snapshot.busy), bridge);
            let current = snapshot
                .current
                .and_then(|id| snapshot.queue.iter().position(|(item, _)| *item == id));
            let queue: Vec<_> = current.map_or_else(Vec::new, |index| {
                snapshot.queue[index + 1..]
                    .iter()
                    .map(|(_, track)| track.clone())
                    .collect()
            });
            apply(
                ui,
                Event::QueueOrder {
                    entries: snapshot.queue,
                    current: snapshot.current,
                },
                bridge,
            );
            apply(ui, Event::Queue(queue), bridge);
            apply(ui, Event::Player(snapshot.player), bridge);
            if let Some(position) = snapshot.position {
                apply(ui, Event::Position(position), bridge);
            }
            ui.global::<Ui>().set_playlist_busy(playlist_busy);
            if let Some(notice) = playlist_notice {
                ui.global::<Ui>().set_error(notice.into());
            }
            ui.global::<Ui>().set_radio_ready(radio_ready);
            ui.global::<Ui>().set_radio_busy(radio_busy);
            ui.global::<Ui>()
                .set_radio_notice(radio_notice.unwrap_or_default().into());
            TUNING_SYNCED.with(|synced| synced.set(false));
            apply(ui, Event::Wave(Box::new(wave)), bridge);
        }
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
        Event::Header(header) => {
            let Header {
                id,
                subtitle,
                subtitle_id,
                liked,
                likeable,
                deletable,
                editable,
                playlist_revision,
            } = header;
            let library = ui.global::<Library>();
            library.set_header_id(id.into());
            library.set_subtitle(subtitle.into());
            library.set_subtitle_id(subtitle_id.into());
            library.set_liked(liked);
            library.set_likeable(likeable);
            library.set_deletable(deletable);
            library.set_editable(editable);
            library.set_playlist_revision(
                playlist_revision
                    .map(|r| r.to_string())
                    .unwrap_or_default()
                    .into(),
            );
        }
        Event::CollectionLiked(id, liked) => {
            if ui.global::<Library>().get_header_id().as_str() == id.to_string() {
                ui.global::<Library>().set_liked(liked);
            }
        }
        Event::Cards(cards) => {
            bridge
                .cards
                .set_vec(cards.into_iter().map(card_item).collect::<Vec<_>>());
            let (first, count) = bridge.card_window.get();
            bridge.update_cards(first, count, true);
        }
        Event::Thumbnail(key, cover, cancel) => {
            if !cancel.load(std::sync::atomic::Ordering::Acquire) {
                bridge.thumbnail(ui, &key, &cover);
            }
        }
        Event::QueueOrder { entries, current } => {
            let same_order = {
                let ids = bridge.queue_ids.borrow();
                ids.len() == entries.len()
                    && ids.iter().zip(&entries).all(|(old, (id, _))| old == id)
            };
            if same_order {
                let old_current = bridge.queue_current.replace(current);
                if old_current != current {
                    let model = ui.global::<Library>().get_queue_order();
                    for (index, id) in bridge.queue_ids.borrow().iter().enumerate() {
                        if (Some(*id) == old_current || Some(*id) == current)
                            && let Some(mut row) = model.row_data(index)
                        {
                            row.current = Some(*id) == current;
                            model.set_row_data(index, row);
                        }
                    }
                }
                return;
            }
            *bridge.queue_ids.borrow_mut() = entries.iter().map(|(id, _)| *id).collect();
            bridge.queue_current.set(current);
            let items = entries
                .into_iter()
                .map(|(id, track)| QueueItem {
                    id: id.to_string().into(),
                    title: track.title.as_str().into(),
                    artist: track
                        .artists
                        .iter()
                        .map(|artist| artist.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                        .into(),
                    current: current == Some(id),
                })
                .collect::<Vec<_>>();
            ui.global::<Library>()
                .set_queue_order(Rc::new(VecModel::from(items)).into());
        }
        Event::Queue(tracks) => {
            let keys = tracks
                .iter()
                .map(|t| format!("t:{}", t.id))
                .collect::<Vec<_>>();
            let ids = bridge.queue_ids.borrow();
            let start = bridge
                .queue_current
                .get()
                .and_then(|id| ids.iter().position(|entry| *entry == id))
                .map_or(ids.len(), |index| index + 1);
            let items = tracks
                .into_iter()
                .enumerate()
                .map(|(index, track)| {
                    let mut item = track_item(LibraryItem {
                        sources: Vec::new(),
                        blocked_reason: None,
                        track,
                        liked: false,
                    });
                    item.occurrence = ids
                        .get(start + index)
                        .map_or_else(String::new, u64::to_string)
                        .into();
                    item
                })
                .collect::<Vec<_>>();
            ui.global::<Library>()
                .set_queue(Rc::new(VecModel::from(items)).into());
            if ui.global::<Ui>().get_list_covers()
                && ui.global::<Ui>().get_panel_open()
                && !keys.is_empty()
            {
                let _ = bridge.sender.send(Command::Thumbnails(keys));
            }
        }
        Event::View(view) => {
            let library = ui.global::<Library>();
            library.set_title("".into());
            library.set_cover(slint::Image::default());
            library.set_header_id("".into());
            library.set_subtitle("".into());
            library.set_subtitle_id("".into());
            library.set_likeable(false);
            library.set_deletable(false);
            library.set_editable(false);
            library.set_playlist_revision("".into());
            ui.global::<Ui>().set_confirm_delete(false);
            bridge.cards.set_vec(Vec::new());
            // Session reconciles the next library against its previous rows. Keep the
            // same rows here so its subsequent indexed deltas still apply.
            bridge.clear_rows();
            bridge.row_window.set((0, 0));
            bridge.card_window.set((0, 0));
            ui.global::<Ui>().set_route(route(view));
            ui.global::<Ui>().set_error("".into());
        }
        Event::PlaylistCreated(_) => {
            ui.global::<Ui>().set_create_playlist(false);
            ui.global::<Ui>().set_playlist_name("".into());
        }
        Event::PlaylistDeleted(_) => ui.global::<Ui>().set_confirm_delete(false),
        Event::PlaylistMutationBusy(busy) => ui.global::<Ui>().set_playlist_busy(busy),
        Event::PlaylistMutationFailed(message) => ui.global::<Ui>().set_error(message.into()),
        Event::RadioState {
            ready,
            busy,
            notice,
        } => {
            ui.global::<Ui>().set_radio_ready(ready);
            ui.global::<Ui>().set_radio_busy(busy);
            ui.global::<Ui>()
                .set_radio_notice(notice.unwrap_or_default().into());
        }
        Event::Wave(state) => apply_wave(ui, bridge, *state),
        Event::Busy(busy) => ui.global::<Ui>().set_busy(busy),
        Event::Player(state) => {
            let player = ui.global::<Player>();
            player.set_status(match state.status {
                State::Idle => Status::Idle,
                State::Loading => Status::Loading,
                State::Playing => Status::Playing,
                State::Paused => Status::Paused,
                State::Buffering => Status::Buffering,
            });
            player.set_quality(
                if state.track.is_none() {
                    String::new()
                } else {
                    state
                        .bitrate_kbps
                        .map_or_else(String::new, |value| format!("{value} кбит/с"))
                }
                .into(),
            );
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
                player.set_album_id(
                    track
                        .album
                        .as_ref()
                        .map_or_else(String::new, ToString::to_string)
                        .into(),
                );
                player.set_album(track.album_title.as_deref().unwrap_or_default().into());
                player.set_artist_id(
                    track
                        .artists
                        .first()
                        .map_or_else(String::new, |a| a.id.to_string())
                        .into(),
                );
                player.set_title(track.title.as_str().into());
                // The provider's own name comes from the connected list; an unknown key shows as is.
                let key = track.id.provider();
                let providers = ui.global::<Ui>().get_providers();
                let name = providers
                    .iter()
                    .find(|p| p.id == key)
                    .map_or_else(|| key.into(), |p| p.name);
                player.set_source(name);
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
            } else {
                player.set_track_id("".into());
                player.set_album_id("".into());
                player.set_album("".into());
                player.set_artist_id("".into());
                player.set_title("".into());
                player.set_artist("".into());
                player.set_source("".into());
                player.set_length(0.);
                player.set_shown(0.);
                player.set_cover(slint::Image::default());
                *anchor.lock().unwrap_or_else(|p| p.into_inner()) = None;
            }
        }
        Event::Position(position) => {
            ui.global::<Player>().set_shown(
                eagle_app::session::position_now(&position, Instant::now()).as_secs_f32(),
            );
            *anchor.lock().unwrap_or_else(|p| p.into_inner()) = Some(position);
        }
        Event::SearchFailed { id, message } => {
            if ui.global::<Ui>().get_route() == Route::Search && id == bridge.search_id.get() {
                ui.global::<Ui>().set_search_pending(false);
                ui.global::<Ui>().set_error(message.into());
            }
        }
        Event::SearchResults { id, rows, cards } => {
            if ui.global::<Ui>().get_route() != Route::Search || id != bridge.search_id.get() {
                return;
            }
            *bridge.search_rows.borrow_mut() = rows;
            *bridge.search_cards.borrow_mut() = cards;
            bridge.filter_search(ui);
            ui.global::<Ui>().set_search_pending(false);
        }
        Event::Library(delta) => {
            if ui.global::<Ui>().get_route() == Route::Search {
                let mut items = bridge.search_rows.borrow_mut();
                for change in delta {
                    let len = items.len();
                    // The session indexes its own list, which can differ from the one shown
                    // here; a stale index must never abort the UI thread.
                    match change {
                        LibraryDelta::Insert { index, item } if index <= len => {
                            items.insert(index, item)
                        }
                        LibraryDelta::Update { index, item } if index < len => items[index] = item,
                        LibraryDelta::Remove { index } if index < len => {
                            items.remove(index);
                        }
                        _ => {
                            log::warn!("search list change ignored: index out of range, len={len}")
                        }
                    }
                }
                drop(items);
                bridge.filter_search(ui);
                return;
            }
            let mut structural = false;
            if delta.iter().any(|change| {
                matches!(
                    change,
                    LibraryDelta::Insert { .. } | LibraryDelta::Remove { .. }
                )
            }) {
                bridge.clear_rows();
            }
            for change in delta {
                match change {
                    LibraryDelta::Insert { index, item } => {
                        structural = true;
                        rows.insert(index, track_item(item));
                    }
                    LibraryDelta::Update { index, item } => {
                        let mut row = track_item(item);
                        // Keep loaded artwork across like toggles: no flicker.
                        if let Some(old) = rows.row_data(index)
                            && old.id == row.id
                        {
                            row.cover = old.cover;
                        }
                        rows.set_row_data(index, row);
                    }
                    LibraryDelta::Remove { index } => {
                        structural = true;
                        rows.remove(index);
                    }
                }
            }
            if structural {
                bridge.refresh_rows(ui);
            }
        }
        Event::LibraryReset(items) => {
            if ui.global::<Ui>().get_route() == Route::Search {
                *bridge.search_rows.borrow_mut() = items;
                bridge.filter_search(ui);
                return;
            }
            bridge.clear_rows();
            rows.set_vec(items.into_iter().map(track_item).collect::<Vec<_>>());
            bridge.refresh_rows(ui);
        }
        Event::Storage {
            staging_bytes,
            other_bytes,
            total_bytes,
            cache_bytes,
            saved_bytes,
            limit,
            folder,
        } => {
            let storage = ui.global::<Storage>();
            storage.set_staging_usage(format!("{:.1} МиБ", staging_bytes as f64 / 1048576.).into());
            storage.set_other_usage(format!("{:.1} МиБ", other_bytes as f64 / 1048576.).into());
            storage.set_total_usage(format!("{:.1} МиБ", total_bytes as f64 / 1048576.).into());
            storage.set_cache_usage(format!("{:.1} МиБ", cache_bytes as f64 / 1048576.).into());
            storage.set_saved_usage(format!("{:.1} МиБ", saved_bytes as f64 / 1048576.).into());
            storage.set_folder(folder.to_string_lossy().as_ref().into());
            storage.set_cache_limit(if limit <= 512 * 1024 * 1024 {
                0
            } else if limit <= 2 * 1024 * 1024 * 1024 {
                1
            } else {
                2
            });
        }
        Event::StorageMoving(moving) => ui.global::<Storage>().set_moving(moving),
        Event::NetworkPolicy(policy) => {
            use eagle_app::session::NetworkStatus;
            let storage = ui.global::<Storage>();
            storage.set_allow_metered(policy.allow_metered);
            storage.set_network_hint(
                match policy.metered {
                    NetworkStatus::Yes => {
                        "Сеть лимитная; настройка влияет только на фоновые загрузки"
                    }
                    NetworkStatus::No => "Сеть без лимита",
                    NetworkStatus::Unknown => "Лимит сети неизвестен; фоновые загрузки разрешены",
                }
                .into(),
            );
        }
        Event::Downloads(jobs) => {
            let items = jobs
                .into_iter()
                .map(|job| DownloadItem {
                    id: job.id.to_string().into(),
                    track: job.title.unwrap_or_else(|| job.track.to_string()).into(),
                    quality: match job.quality {
                        Quality::High => "320 кбит/с",
                        Quality::Standard => "До 192 кбит/с",
                    }
                    .into(),
                    status: match job.state {
                        DownloadState::Queued => "В очереди",
                        DownloadState::Running => "Сохраняется",
                        DownloadState::Paused => "На паузе",
                        DownloadState::Cancelled => "Отменено",
                        DownloadState::Failed => "Ошибка",
                        DownloadState::AudioReady => "Подготовка обложки",
                        DownloadState::Saved => "Сохранено",
                    }
                    .into(),
                    error: job.error.unwrap_or_default().into(),
                    pausable: matches!(
                        job.state,
                        DownloadState::Queued | DownloadState::Running | DownloadState::AudioReady
                    ),
                    retryable: matches!(
                        job.state,
                        DownloadState::Paused | DownloadState::Failed | DownloadState::Cancelled
                    ),
                    saved: job.state == DownloadState::Saved,
                    cancellable: matches!(
                        job.state,
                        DownloadState::Queued
                            | DownloadState::Running
                            | DownloadState::Paused
                            | DownloadState::Failed
                            | DownloadState::AudioReady
                    ),
                })
                .collect::<Vec<_>>();
            ui.global::<Storage>()
                .set_jobs(Rc::new(VecModel::from(items)).into());
        }
        Event::Error(message) => ui.global::<Ui>().set_error(message.into()),
        Event::Cover(target, id, cover) => {
            if matches!(target, eagle_app::session::CoverTarget::Collection) {
                ui.global::<Library>().set_cover(bridge.image(&cover));
                return;
            }
            if ui.global::<Player>().get_track_id().as_str() == id.to_string() {
                ui.global::<Player>().set_cover(bridge.image(&cover));
                COVER_COLOR.with(|c| *c.borrow_mut() = cover.dominant);
                apply_colors(ui);
            }
        }
        Event::Account => {
            ui.global::<Ui>().set_signed_in(true);
            ui.global::<Player>().set_cover(slint::Image::default());
            ui.global::<Library>().set_cover(slint::Image::default());
            bridge.clear_rows();
            for index in 0..bridge.cards.row_count() {
                if let Some(mut card) = bridge.cards.row_data(index) {
                    card.cover = slint::Image::default();
                    bridge.cards.set_row_data(index, card);
                }
            }
        }
        // Until the user answers in the services section, nothing is sent to the provider.
        Event::LikesBackfillOffer { provider, count } => {
            bridge
                .services
                .offers
                .borrow_mut()
                .insert(provider, Offer::Asking(count));
            bridge.services.publish(ui);
        }
        Event::ServicesIntro => {
            // Only the start screen is replaced; a screen the user already opened stays.
            if ui.global::<Ui>().get_route() == Route::Library {
                let _ = bridge.sender.send(Command::Downloads);
                let _ = bridge.sender.send(Command::Open(View::Settings));
            }
        }
        Event::SignIn { provider, state } => {
            bridge.services.sign_in.borrow_mut().insert(provider, state);
            bridge.services.publish(ui);
        }
        Event::Providers(all) => {
            let searchable = all
                .iter()
                .filter(|p| {
                    p.signed_in
                        && (p.capabilities.search_tracks
                            || p.capabilities.search_artists
                            || p.capabilities.search_albums
                            || p.capabilities.search_playlists)
                })
                .map(|p| ProviderItem {
                    id: p.id.as_str().into(),
                    name: p.name.as_str().into(),
                    ..Default::default()
                })
                .collect::<Vec<_>>();
            ui.global::<Ui>()
                .set_searchable_providers(Rc::new(VecModel::from(searchable)).into());
            // A service that signed out forgets the answer about its likes.
            bridge
                .services
                .offers
                .borrow_mut()
                .retain(|id, _| all.iter().any(|p| p.signed_in && p.id == *id));
            *bridge.services.statuses.borrow_mut() = all;
            bridge.services.publish(ui);
        }
    }
}

thread_local! { static COVER_COLOR: RefCell<[u8; 3]> = const { RefCell::new([128; 3]) }; }
pub(super) fn apply_colors(ui: &DesktopWindow) {
    let tokens = ui.global::<Theme>().get_t();
    let rgb = |c: slint::Color| [c.red(), c.green(), c.blue()];
    let background = rgb(tokens.bg);
    let surface = rgb(tokens.surf);
    let tint = match ui.global::<Theme>().get_tint() {
        0 => eagle_palette::Tint::Off,
        2 => eagle_palette::Tint::Bright,
        _ => eagle_palette::Tint::Soft,
    };
    let (bar, panel) = COVER_COLOR
        .with(|c| eagle_palette::player_surfaces(*c.borrow(), background, surface, tint));
    let color = |rgb: [u8; 3]| slint::Color::from_rgb_u8(rgb[0], rgb[1], rgb[2]);
    ui.global::<Player>().set_colors(PlayerColors {
        bg: color(panel.background),
        bar: color(bar.background),
        tx: color(panel.text),
        tx2: color(panel.secondary),
        ln: color(bar.line),
        accent: color(panel.accent),
    });
    let pad = COVER_COLOR.with(|c| eagle_palette::wave_pad(*c.borrow(), background));
    ui.global::<Wave>().set_colors(WaveColors {
        pad: color(pad.background),
        ring: color(pad.ring),
        tx: color(pad.text),
        tx2: color(pad.secondary),
        dot: color(pad.dot),
        card: color(pad.card),
    });
    #[cfg(target_os = "macos")]
    super::window_chrome::apply(ui);
}

thread_local! { static UI_BRIDGE: RefCell<Option<Rc<Bridge>>> = const { RefCell::new(None) }; }
fn with_bridge(f: impl FnOnce(&Bridge)) {
    let bridge = UI_BRIDGE.with(|b| b.borrow().clone());
    if let Some(bridge) = bridge {
        f(&bridge);
    }
}

/// Flips a track heart at once; the session's reply confirms or reverts it.
fn like_optimistically(ui: &DesktopWindow, rows: &VecModel<TrackItem>, id: &str) {
    for index in 0..rows.row_count() {
        if let Some(mut row) = rows.row_data(index)
            && row.id.as_str() == id
        {
            row.liked = !row.liked;
            rows.set_row_data(index, row);
            break;
        }
    }
    let player = ui.global::<Player>();
    if player.get_track_id().as_str() == id {
        player.set_liked(!player.get_liked());
    }
}

pub(super) fn bind(ui: &DesktopWindow, sender: CommandSender) {
    ui.global::<Wave>()
        .set_dots(Rc::new(VecModel::from(super::pad_dots())).into());
    let actions = ui.global::<Actions>();
    let tx = sender.clone();
    actions.on_refresh_playlist(move |id| {
        if let Ok(id) = id.parse::<eagle_core::TrackId>() {
            let _ = tx.send(Command::ClearPlaylistMutationNotice(id.clone()));
            let _ = tx.send(Command::Playlist(id));
        }
    });
    let tx = sender.clone();
    actions.on_edit_playlist(move |id, revision, from, action| {
        let (Ok(id), Ok(revision), Ok(from)) =
            (id.parse(), revision.parse(), usize::try_from(from))
        else {
            return;
        };
        let edit = match action {
            0 => from
                .checked_add(1)
                .map(|to| eagle_core::PlaylistEdit::Delete { from, to }),
            1 => from
                .checked_sub(1)
                .map(|to| eagle_core::PlaylistEdit::Move { from, to }),
            2 => from
                .checked_add(1)
                .map(|to| eagle_core::PlaylistEdit::Move { from, to }),
            _ => None,
        };
        if let Some(edit) = edit {
            let _ = tx.send(Command::EditPlaylist { id, revision, edit });
        }
    });
    let tx = sender.clone();
    actions.on_add_track_to_playlist(move |id, revision, track, album| {
        if let (Ok(id), Ok(revision), Ok(track_id), Ok(album_id)) =
            (id.parse(), revision.parse(), track.parse(), album.parse())
        {
            let _ = tx.send(Command::AddTrackToPlaylist {
                id,
                revision,
                entry: Box::new(eagle_core::PlaylistTrack { track_id, album_id }),
            });
        }
    });
    let tx = sender.clone();
    let weak = ui.as_weak();
    actions.on_save(move |id| {
        if let (Ok(id), Some(ui)) = (id.parse(), weak.upgrade()) {
            let quality = if ui.global::<Storage>().get_quality() == 0 {
                Quality::Standard
            } else {
                Quality::High
            };
            let _ = tx.send(Command::Save { id, quality });
        }
    });
    let tx = sender.clone();
    let weak = ui.as_weak();
    actions.on_save_all(move || {
        if let Some(ui) = weak.upgrade() {
            let quality = if ui.global::<Storage>().get_quality() == 0 {
                Quality::Standard
            } else {
                Quality::High
            };
            let _ = tx.send(Command::SaveAll(quality));
        }
    });
    let tx = sender.clone();
    actions.on_download_control(move |id, action| {
        if let Ok(id) = id.parse() {
            let command = match action {
                0 => Command::PauseDownload(id),
                1 => Command::RetryDownload(id),
                2 => Command::CancelDownload(id),
                _ => return,
            };
            let _ = tx.send(command);
        }
    });
    let tx = sender.clone();
    actions.on_downloads(move || {
        let _ = tx.send(Command::Downloads);
    });
    let tx = sender.clone();
    actions.on_remove_saved(move |id| {
        if let Ok(id) = id.parse() {
            let _ = tx.send(Command::RemoveSaved(id));
        }
    });
    let tx = sender.clone();
    actions.on_allow_metered(move |allow| {
        let _ = tx.send(Command::AllowMetered(allow));
    });
    let tx = sender.clone();
    actions.on_cache_limit(move |choice| {
        let limit = match choice {
            0 => 512 * 1024 * 1024,
            1 => 2 * 1024 * 1024 * 1024,
            2 => 8 * 1024 * 1024 * 1024,
            _ => return,
        };
        let _ = tx.send(Command::CacheLimit(limit));
    });
    let tx = sender.clone();
    actions.on_jump_to(move |id| {
        if let Ok(id) = id.parse() {
            let _ = tx.send(Command::JumpTo(id));
        }
    });
    let tx = sender.clone();
    actions.on_enqueue_next(move |id| {
        if let Ok(id) = id.parse() {
            let _ = tx.send(Command::EnqueueNext(id));
        }
    });
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
    simple!(on_radio_start, Command::RadioPlay);
    simple!(on_wave_not_this, Command::RadioNotThis);
    simple!(on_wave_more_like_this, Command::RadioMoreLikeThis);
    let tx = sender.clone();
    actions.on_wave_tune(move |x, y, novelty| {
        let _ = tx.send(Command::RadioTune {
            mood: Some((x, y)),
            novelty,
        });
    });
    let tx = sender.clone();
    actions.on_play(move |id| {
        if let Ok(id) = id.parse() {
            let _ = tx.send(Command::Play(id));
        }
    });
    let tx = sender.clone();
    let weak = ui.as_weak();
    actions.on_play_row(move |index| {
        if let Ok(index) = usize::try_from(index) {
            if let Some(ui) = weak.upgrade()
                && ui.global::<Ui>().get_route() == Route::Search
            {
                with_bridge(|bridge| {
                    if let Some(row) = bridge.rows.row_data(index)
                        && let Ok(id) = row.id.parse()
                    {
                        let _ = tx.send(Command::Play(id));
                    }
                });
            } else {
                let _ = tx.send(Command::PlayRow(index));
            }
        }
    });
    let tx = sender.clone();
    let weak = ui.as_weak();
    actions.on_like(move |id| {
        let Ok(parsed) = id.parse() else {
            return;
        };
        if let Some(ui) = weak.upgrade() {
            with_bridge(|bridge| like_optimistically(&ui, &bridge.rows, &id));
        }
        let _ = tx.send(Command::Like(parsed));
    });
    let tx = sender.clone();
    let weak = ui.as_weak();
    actions.on_like_collection(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let library = ui.global::<Library>();
        let Ok(id) = library.get_header_id().parse() else {
            return;
        };
        library.set_liked(!library.get_liked());
        let _ = tx.send(match ui.global::<Ui>().get_route() {
            Route::Artist => Command::LikeArtist(id),
            _ => Command::LikeAlbum(id),
        });
    });
    let weak = ui.as_weak();
    actions.on_visible_rows(move |first, count| {
        if let Some(ui) = weak.upgrade() {
            with_bridge(|bridge| {
                bridge.rows_visible(&ui, first.max(0) as usize, count.max(0) as usize);
                bridge.release_unused_images(&ui);
            });
        }
    });
    let weak = ui.as_weak();
    actions.on_list_covers_changed(move || {
        if let Some(ui) = weak.upgrade() {
            with_bridge(|bridge| bridge.list_covers_changed(&ui));
        }
    });
    let weak = ui.as_weak();
    actions.on_panel_content({
        let weak = ui.as_weak();
        move |open| {
            if let Some(ui) = weak.upgrade() {
                let queue = ui.global::<Library>().get_queue();
                if open && ui.global::<Ui>().get_list_covers() {
                    let keys = queue
                        .iter()
                        .map(|row| format!("t:{}", row.id))
                        .collect::<Vec<_>>();
                    with_bridge(|bridge| {
                        let _ = bridge.sender.send(Command::Thumbnails(keys));
                    });
                } else {
                    for index in 0..queue.row_count() {
                        if let Some(mut row) = queue.row_data(index) {
                            row.cover = slint::Image::default();
                            queue.set_row_data(index, row);
                        }
                    }
                    with_bridge(|bridge| bridge.release_unused_images(&ui));
                }
            }
        }
    });
    actions.on_visible_cards(move |first, count| {
        if let Some(ui) = weak.upgrade() {
            with_bridge(|bridge| {
                bridge.cards_visible(first.max(0) as usize, count.max(0) as usize);
                bridge.release_unused_images(&ui);
            });
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
    let weak = ui.as_weak();
    actions.on_search_changed(move |text| {
        if let Some(ui) = weak.upgrade() {
            with_bridge(|bridge| {
                bridge.search_id.set(bridge.search_id.get().wrapping_add(1));
                if text.trim().is_empty() {
                    bridge.search_rows.borrow_mut().clear();
                    bridge.search_cards.borrow_mut().clear();
                    bridge.rows.set_vec(Vec::new());
                    bridge.cards.set_vec(Vec::new());
                }
            });
            let valid = text.trim().chars().count() >= 2;
            ui.global::<Ui>().set_search_valid(valid);
            ui.global::<Ui>().set_search_pending(valid);
        }
        let _ = tx.send(Command::InvalidateSearch);
        if text.trim().is_empty() {
            with_bridge(|bridge| {
                let _ = tx.send(Command::SearchUi {
                    id: bridge.search_id.get(),
                    kind: 0,
                    query: String::new(),
                });
            });
        }
    });
    let weak = ui.as_weak();
    actions.on_search_provider_changed(move || {
        if let Some(ui) = weak.upgrade() {
            with_bridge(|bridge| bridge.filter_search(&ui));
        }
    });
    let tx = sender.clone();
    let weak = ui.as_weak();
    actions.on_search(move |text| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let query = text.to_string();
        if query.trim().chars().count() < 2 {
            return;
        }
        with_bridge(|bridge| {
            let _ = tx.send(Command::SearchUi {
                id: bridge.search_id.get(),
                kind: ui.global::<Ui>().get_search_kind() as u8,
                query,
            });
        });
    });
    actions.on_valid_playlist_name(|name| !name.trim().is_empty());
    let tx = sender.clone();
    let weak = ui.as_weak();
    actions.on_create_playlist(move |name| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if !ui.global::<Ui>().get_playlist_busy() && !name.trim().is_empty() {
            ui.global::<Ui>().set_error("".into());
            ui.global::<Ui>().set_playlist_busy(true);
            if tx
                .send(Command::CreatePlaylist(name.trim().to_owned()))
                .is_err()
            {
                ui.global::<Ui>().set_playlist_busy(false);
                ui.global::<Ui>()
                    .set_error("Не удалось создать плейлист".into());
            }
        }
    });
    let tx = sender.clone();
    let weak = ui.as_weak();
    actions.on_delete_playlist(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if !ui.global::<Ui>().get_playlist_busy()
            && ui.global::<Ui>().get_confirm_delete()
            && let Ok(id) = ui.global::<Library>().get_header_id().parse()
        {
            ui.global::<Ui>().set_error("".into());
            ui.global::<Ui>().set_playlist_busy(true);
            if tx.send(Command::DeletePlaylist(id)).is_err() {
                ui.global::<Ui>().set_playlist_busy(false);
                ui.global::<Ui>()
                    .set_error("Не удалось удалить плейлист".into());
            }
        }
    });
    let tx = sender.clone();
    actions.on_login(move |provider, token| {
        let _ = tx.send(Command::Login {
            provider: provider.to_string(),
            token: eagle_app::session::Token::new(token.to_string()),
        });
    });
    let tx = sender.clone();
    actions.on_sign_in_start(move |provider, method| {
        let _ = tx.send(Command::SignInStart {
            provider: provider.to_string(),
            method: usize::try_from(method).unwrap_or_default(),
        });
    });
    let tx = sender.clone();
    actions.on_sign_in_password(move |provider, login, password| {
        let _ = tx.send(Command::SignInPassword {
            provider: provider.to_string(),
            login: login.to_string(),
            password: eagle_app::session::Token::new(password.to_string()),
        });
    });
    let tx = sender.clone();
    actions.on_sign_in_second_factor(move |provider, code| {
        // Codes are shown as «000 000»; the provider gets the digits.
        let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
        let _ = tx.send(Command::SignInSecondFactor {
            provider: provider.to_string(),
            code: eagle_app::session::Token::new(code),
        });
    });
    let tx = sender.clone();
    actions.on_sign_in_cancel(move |provider| {
        let _ = tx.send(Command::SignInCancel {
            provider: provider.to_string(),
        });
    });
    let tx = sender.clone();
    let weak = ui.as_weak();
    actions.on_likes_backfill(move |provider, accept| {
        let _ = tx.send(Command::LikesBackfill {
            provider: provider.to_string(),
            accept,
        });
        if let Some(ui) = weak.upgrade() {
            with_bridge(|bridge| {
                bridge.services.offers.borrow_mut().insert(
                    provider.to_string(),
                    if accept { Offer::Sent } else { Offer::Skipped },
                );
                bridge.services.publish(&ui);
            });
        }
    });
    actions.on_open_url(|url| {
        crate::browser::open(&url);
    });
    let tx = sender.clone();
    actions.on_logout(move |provider| {
        let _ = tx.send(Command::Logout {
            provider: provider.to_string(),
            keep_saved: false,
        });
    });
    let tx = sender.clone();
    let weak = ui.as_weak();
    actions.on_collection(move |route, id| {
        if let Ok(id) = id.parse() {
            if let Some(ui) = weak.upgrade() {
                ui.global::<Ui>().set_create_playlist(false);
            }
            let _ = tx.send(match route {
                Route::Album => Command::Album(id),
                Route::Artist => Command::Artist(id),
                _ => Command::Playlist(id),
            });
        }
    });
    let weak = ui.as_weak();
    actions.on_open(move |route| {
        if let Some(ui) = weak.upgrade() {
            ui.global::<Ui>().set_create_playlist(false);
        }
        let view = match route {
            Route::Saved => View::Saved,
            Route::History => View::History,
            Route::Library => View::Library,
            Route::Search => View::Search,
            Route::Album => View::Album,
            Route::Playlist => View::Playlist,
            Route::Settings => View::Settings,
            Route::Albums => View::Albums,
            Route::Artists => View::Artists,
            Route::Artist => View::Artist,
            Route::Radio => View::Radio,
        };
        if route == Route::Settings {
            let _ = sender.send(Command::Downloads);
        }
        let _ = sender.send(Command::Open(view));
    });
}

pub(super) fn initialize(
    rows: Rc<VecModel<TrackItem>>,
    cards: Rc<VecModel<CardItem>>,
    anchor: Arc<Mutex<Option<PositionAnchor>>>,
    sender: CommandSender,
) {
    TUNING_SYNCED.with(|synced| synced.set(false));
    UI_BRIDGE.with(|bridge| {
        *bridge.borrow_mut() = Some(Rc::new(Bridge {
            services: Services::default(),
            rows,
            search_rows: RefCell::new(Vec::new()),
            search_cards: RefCell::new(Vec::new()),
            search_id: Cell::new(0),
            cards,
            anchor,
            sender,
            row_window: Cell::new((0, 0)),
            card_window: Cell::new((0, 0)),
            images: RefCell::new(HashMap::new()),
            queue_ids: RefCell::new(Vec::new()),
            queue_current: Cell::new(None),
        }))
    });
}
pub(super) fn deliver(ui: &DesktopWindow, event: Event) {
    with_bridge(|bridge| {
        apply(ui, event, bridge);
        bridge.release_unused_images(ui);
    });
}
pub(super) fn retry_artwork(ui: &DesktopWindow) {
    with_bridge(|bridge| {
        let (first, count) = bridge.row_window.get();
        bridge.update_rows(ui, first, count, true);
        let (first, count) = bridge.card_window.get();
        bridge.update_cards(first, count, true);
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
pub(super) mod tests {
    use super::*;
    #[test]
    fn likes_are_counted_in_russian() {
        assert_eq!(likes_phrase(1), "1 лайк");
        assert_eq!(likes_phrase(2), "2 лайка");
        assert_eq!(likes_phrase(5), "5 лайков");
        assert_eq!(likes_phrase(11), "11 лайков");
        assert_eq!(likes_phrase(12), "12 лайков");
        assert_eq!(likes_phrase(21), "21 лайк");
        assert_eq!(likes_phrase(104), "104 лайка");
        assert_eq!(likes_phrase(128), "128 лайков");
    }
    #[test]
    fn link_label_drops_scheme_and_trailing_slash() {
        assert_eq!(link_label("https://ya.ru/device"), "ya.ru/device");
        assert_eq!(link_label("https://example.test/"), "example.test");
    }
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
    #[test]
    fn search_stale_results_are_dropped_and_previous_results_stay_until_new_ones_arrive() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = DesktopWindow::new().unwrap();
        ui.global::<Ui>().set_route(Route::Search);
        let root = std::env::temp_dir().join(format!("eagle-ui-search-{}", std::process::id()));
        let session = eagle_app::session::Session::start(root, |_| {}).unwrap();
        let bridge = Bridge {
            services: Services::default(),
            rows: Rc::new(VecModel::default()),
            search_rows: RefCell::new(Vec::new()),
            search_cards: RefCell::new(Vec::new()),
            search_id: Cell::new(1),
            cards: Rc::new(VecModel::default()),
            anchor: Arc::new(Mutex::new(None)),
            sender: session.sender(),
            row_window: Cell::new((0, 0)),
            card_window: Cell::new((0, 0)),
            images: RefCell::new(HashMap::new()),
            queue_ids: RefCell::new(Vec::new()),
            queue_current: Cell::new(None),
        };
        let item = |id: &str, sources: Vec<&str>| LibraryItem {
            sources: sources.into_iter().map(|id| id.parse().unwrap()).collect(),
            blocked_reason: None,
            track: Arc::new(
                serde_json::from_value(serde_json::json!({
                    "id": id, "title": id, "artists": [], "album": null, "album_title": null,
                    "duration_ms": 0, "cover": null, "availability": "Available"
                }))
                .unwrap(),
            ),
            liked: false,
        };
        apply(
            &ui,
            Event::SearchResults {
                id: 1,
                rows: vec![item("fakea:old", vec![])],
                cards: vec![],
            },
            &bridge,
        );
        let mut batch = crate::events::Batch::default();
        batch.push(crate::events::Envelope {
            event: Event::SearchResults {
                id: 1,
                rows: vec![item("fakea:late", vec![])],
                cards: vec![],
            },
            epoch: 0,
            last_snapshot: false,
        });
        bridge.search_id.set(2);
        assert_eq!(bridge.rows.row_data(0).unwrap().id.as_str(), "fakea:old");
        apply(&ui, batch.pop().unwrap().event, &bridge);
        assert_eq!(bridge.rows.row_data(0).unwrap().id.as_str(), "fakea:old");
        apply(
            &ui,
            Event::SearchResults {
                id: 2,
                rows: vec![
                    item("fakea:a", vec!["fakeb:a"]),
                    item("fakea:b", vec![]),
                    item("fakeb:c", vec![]),
                ],
                cards: vec![],
            },
            &bridge,
        );
        ui.global::<Ui>().set_search_provider("fakeb".into());
        bridge.filter_search(&ui);
        assert_eq!(bridge.rows.row_count(), 2);
        assert_eq!(bridge.rows.row_data(0).unwrap().id.as_str(), "fakea:a");
        assert_eq!(bridge.rows.row_data(1).unwrap().id.as_str(), "fakeb:c");
        let card = |id: &str, sources: Vec<&str>| Card {
            id: id.parse().unwrap(),
            key: format!("a:{id}"),
            title: id.into(),
            subtitle: String::new(),
            cover: None,
            round: false,
            sources: sources.into_iter().map(|id| id.parse().unwrap()).collect(),
        };
        apply(
            &ui,
            Event::SearchResults {
                id: 2,
                rows: vec![],
                cards: vec![
                    card("fakea:a", vec!["fakeb:a"]),
                    card("fakea:b", vec![]),
                    card("fakeb:c", vec![]),
                ],
            },
            &bridge,
        );
        assert_eq!(bridge.cards.row_count(), 2);
        assert_eq!(bridge.cards.row_data(0).unwrap().id.as_str(), "fakea:a");
        assert_eq!(bridge.cards.row_data(1).unwrap().id.as_str(), "fakeb:c");
        ui.global::<Ui>().set_search_provider("".into());
        bridge.filter_search(&ui);
        assert_eq!(bridge.cards.row_count(), 3);
    }
    #[test]
    fn search_deltas_outside_the_result_list_are_ignored() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = DesktopWindow::new().unwrap();
        ui.global::<Ui>().set_route(Route::Search);
        let root =
            std::env::temp_dir().join(format!("eagle-ui-search-delta-{}", std::process::id()));
        let session = eagle_app::session::Session::start(root, |_| {}).unwrap();
        let bridge = Bridge {
            services: Services::default(),
            rows: Rc::new(VecModel::default()),
            search_rows: RefCell::new(Vec::new()),
            search_cards: RefCell::new(Vec::new()),
            search_id: Cell::new(1),
            cards: Rc::new(VecModel::default()),
            anchor: Arc::new(Mutex::new(None)),
            sender: session.sender(),
            row_window: Cell::new((0, 0)),
            card_window: Cell::new((0, 0)),
            images: RefCell::new(HashMap::new()),
            queue_ids: RefCell::new(Vec::new()),
            queue_current: Cell::new(None),
        };
        let item = |id: &str| LibraryItem {
            sources: Vec::new(),
            blocked_reason: None,
            track: Arc::new(
                serde_json::from_value(serde_json::json!({
                    "id": id, "title": id, "artists": [], "album": null, "album_title": null,
                    "duration_ms": 0, "cover": null, "availability": "Available"
                }))
                .unwrap(),
            ),
            liked: false,
        };
        // Indexes of a list the search view never showed must not abort the process.
        apply(
            &ui,
            Event::Library(vec![
                LibraryDelta::Remove { index: 7 },
                LibraryDelta::Update {
                    index: 3,
                    item: item("fakea:3"),
                },
                LibraryDelta::Insert {
                    index: 5,
                    item: item("fakea:5"),
                },
            ]),
            &bridge,
        );
        assert_eq!(bridge.search_rows.borrow().len(), 0);
        assert_eq!(bridge.rows.row_count(), 0);
        // Valid deltas still apply.
        apply(
            &ui,
            Event::Library(vec![LibraryDelta::Insert {
                index: 0,
                item: item("fakea:0"),
            }]),
            &bridge,
        );
        assert_eq!(bridge.rows.row_count(), 1);
        drop(session);
    }
    #[test]
    fn search_old_visible_row_never_plays_new_row_before_reply_delivery() {
        i_slint_backend_testing::init_no_event_loop();
        let root =
            std::env::temp_dir().join(format!("eagle-search-play-race-{}", std::process::id()));
        let (tx, rx) = std::sync::mpsc::channel();
        let session = eagle_app::session::Session::start_with_providers(
            root.clone(),
            vec![eagle_app::fixture::provider()],
            move |event| {
                let _ = tx.send(event);
            },
        )
        .unwrap();
        let sender = session.sender();
        let ui = DesktopWindow::new().unwrap();
        ui.global::<Ui>().set_route(Route::Search);
        let rows = Rc::new(VecModel::from(vec![TrackItem {
            id: "fixture:999".into(),
            ..Default::default()
        }]));
        initialize(
            rows,
            Rc::new(VecModel::default()),
            Arc::new(Mutex::new(None)),
            sender.clone(),
        );
        bind(&ui, sender.clone());
        sender.send(Command::Open(View::Search)).unwrap();
        sender
            .send(Command::SearchUi {
                id: 1,
                kind: 0,
                query: "new".into(),
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if matches!(rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap(),
                Event::SearchResults { id: 1, rows, .. } if !rows.is_empty())
            {
                break;
            }
        }
        // The worker has new rows but the UI has not delivered that event yet.
        ui.global::<Actions>().invoke_play_row(0);
        loop {
            match rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap()
            {
                Event::Error(message) => {
                    assert!(message.contains("трек не найден"), "{message}");
                    break;
                }
                Event::Player(player) if player.track.is_some() => panic!("played another row"),
                _ => {}
            }
        }
        clear();
        drop(session);
        std::fs::remove_dir_all(root).unwrap();
    }
    pub(crate) fn structural_changes_and_cover_toggle_release_artwork() {
        let ui = DesktopWindow::new().unwrap();
        let root = std::env::temp_dir().join(format!("eagle-ui-covers-{}", std::process::id()));
        let session = eagle_app::session::Session::start(root, |_| {}).unwrap();
        let rows = Rc::new(VecModel::default());
        let bridge = Bridge {
            services: Services::default(),
            rows: rows.clone(),
            search_rows: RefCell::new(Vec::new()),
            search_cards: RefCell::new(Vec::new()),
            search_id: Cell::new(0),
            cards: Rc::new(VecModel::default()),
            anchor: Arc::new(Mutex::new(None)),
            sender: session.sender(),
            row_window: Cell::new((0, 1)),
            card_window: Cell::new((0, 0)),
            images: RefCell::new(HashMap::new()),
            queue_ids: RefCell::new(Vec::new()),
            queue_current: Cell::new(None),
        };
        let item = |id: &str| LibraryItem {
            sources: Vec::new(),
            blocked_reason: None,
            track: Arc::new(
                serde_json::from_value(serde_json::json!({
                    "id": format!("yandex:{id}"), "title": id, "artists": [],
                    "album": null, "album_title": null, "duration_ms": 0,
                    "cover": null, "availability": "Available"
                }))
                .unwrap(),
            ),
            liked: false,
        };
        let cover = Arc::new(eagle_palette::Cover {
            rgba: vec![255; 4],
            width: 1,
            height: 1,
            dominant: [0; 3],
        });
        apply(
            &ui,
            Event::LibraryReset((0..8).map(|id| item(&id.to_string())).collect()),
            &bridge,
        );
        apply(&ui, Event::View(View::History), &bridge);
        apply(
            &ui,
            Event::Library(vec![LibraryDelta::Remove { index: 7 }]),
            &bridge,
        );
        assert_eq!(rows.row_count(), 7);
        assert_eq!(rows.row_data(6).unwrap().id.as_str(), "yandex:6");
        apply(
            &ui,
            Event::LibraryReset(vec![item("1"), item("2")]),
            &bridge,
        );
        assert_eq!(bridge.image(&cover), bridge.image(&cover));
        bridge.thumbnail(&ui, "t:yandex:1", &cover);
        assert_eq!(bridge.images.borrow().len(), 1);
        assert_eq!(rows.row_data(0).unwrap().cover.size().width, 1);
        apply(
            &ui,
            Event::Library(vec![LibraryDelta::Insert {
                index: 0,
                item: item("3"),
            }]),
            &bridge,
        );
        assert_eq!(rows.row_data(1).unwrap().cover.size().width, 0);
        bridge.release_unused_images(&ui);
        assert!(bridge.images.borrow().is_empty());
        apply(
            &ui,
            Event::Thumbnail(
                "t:yandex:1".into(),
                cover.clone(),
                Arc::new(std::sync::atomic::AtomicBool::new(true)),
            ),
            &bridge,
        );
        assert!(
            bridge.images.borrow().is_empty(),
            "cancelled artwork must not be converted to Slint"
        );
        let entries = vec![(1, item("1").track), (2, item("2").track)];
        apply(
            &ui,
            Event::QueueOrder {
                entries: entries.clone(),
                current: Some(1),
            },
            &bridge,
        );
        let model = ui.global::<Library>().get_queue_order();
        apply(
            &ui,
            Event::QueueOrder {
                entries,
                current: Some(2),
            },
            &bridge,
        );
        assert!(std::ptr::eq(
            model.as_any(),
            ui.global::<Library>().get_queue_order().as_any()
        ));
        assert!(!model.row_data(0).unwrap().current);
        assert!(model.row_data(1).unwrap().current);
        bridge.thumbnail(&ui, "t:yandex:1", &cover);
        apply(
            &ui,
            Event::Library(vec![LibraryDelta::Remove { index: 0 }]),
            &bridge,
        );
        assert_eq!(rows.row_data(0).unwrap().cover.size().width, 0);
        bridge.thumbnail(&ui, "t:yandex:1", &cover);
        apply(&ui, Event::LibraryReset(vec![item("1")]), &bridge);
        assert_eq!(rows.row_data(0).unwrap().cover.size().width, 0);
        bridge.thumbnail(&ui, "t:yandex:1", &cover);
        ui.global::<Ui>().set_list_covers(false);
        bridge.list_covers_changed(&ui);
        bridge.thumbnail(&ui, "t:yandex:1", &cover);
        assert_eq!(rows.row_data(0).unwrap().cover.size().width, 0);
        ui.global::<Ui>().set_list_covers(true);
        bridge.list_covers_changed(&ui);
        bridge.thumbnail(&ui, "t:yandex:1", &cover);
        assert_eq!(rows.row_data(0).unwrap().cover.size().width, 1);
        ui.global::<Library>()
            .set_playlists(Rc::new(VecModel::<CollectionItem>::default()).into());
        let track = item("7").track;
        let snapshot = eagle_app::session::SessionSnapshot {
            view: View::Library,
            rows: vec![item("7")],
            cards: vec![],
            collections: vec![],
            title: "Local".into(),
            length: Duration::from_secs(20),
            header: Header {
                editable: true,
                playlist_revision: Some(u64::MAX),
                ..Header::default()
            },
            busy: false,
            playlist_mutation_busy: true,
            playlist_mutation_notice: Some("Результат изменения неизвестен".into()),
            radio_ready: true,
            radio_busy: false,
            radio_notice: Some("Нет сети; очередь сохранена".into()),
            wave: Default::default(),
            signed_in: true,
            player: Arc::new(eagle_app::session::PlayerState {
                track: Some(track.clone()),
                bitrate_kbps: Some(128),
                status: State::Paused,
                ..Default::default()
            }),
            queue: vec![(1, track.clone()), (2, track)],
            current: Some(1),
            position: Some(PositionAnchor {
                track: 0,
                position: Duration::from_secs(4),
                audible_at: Instant::now(),
                state: State::Paused,
            }),
        };
        apply(&ui, Event::Snapshot(Box::new(snapshot)), &bridge);
        assert!(ui.global::<Ui>().get_playlist_busy());
        assert_eq!(
            ui.global::<Ui>().get_error().as_str(),
            "Результат изменения неизвестен"
        );
        assert!(ui.global::<Ui>().get_radio_ready());
        assert_eq!(
            ui.global::<Ui>().get_radio_notice().as_str(),
            "Нет сети; очередь сохранена"
        );
        assert!(ui.global::<Library>().get_editable());
        assert_eq!(
            ui.global::<Library>().get_playlist_revision().as_str(),
            u64::MAX.to_string()
        );
        assert_eq!(ui.global::<Player>().get_quality().as_str(), "128 кбит/с");
        let queue = ui.global::<Library>().get_queue_order();
        assert_eq!(queue.row_count(), 2);
        assert_eq!(queue.row_data(0).unwrap().id.as_str(), "1");
        assert_eq!(queue.row_data(1).unwrap().id.as_str(), "2");
        assert!(queue.row_data(0).unwrap().current);
        assert!(!queue.row_data(1).unwrap().current);
        apply(
            &ui,
            Event::Storage {
                staging_bytes: 0,
                other_bytes: 0,
                total_bytes: 3145728,
                cache_bytes: 1048576,
                saved_bytes: 2097152,
                limit: 536870912,
                folder: "/tmp/audio".into(),
            },
            &bridge,
        );
        assert_eq!(ui.global::<Storage>().get_cache_usage().as_str(), "1.0 МиБ");
        assert_eq!(ui.global::<Storage>().get_saved_usage().as_str(), "2.0 МиБ");
        assert_eq!(ui.global::<Storage>().get_cache_limit(), 0);
        let unknown = eagle_app::session::PlayerState {
            track: Some(item("7").track),
            ..Default::default()
        };
        apply(&ui, Event::Player(Arc::new(unknown)), &bridge);
        assert_eq!(ui.global::<Player>().get_quality().as_str(), "");
        apply(&ui, Event::Downloads(Vec::new()), &bridge);
        assert_eq!(ui.global::<Storage>().get_jobs().row_count(), 0);
        assert_eq!(rows.row_count(), 1);
        assert_eq!(ui.global::<Player>().get_track_id(), "yandex:7");
        assert_eq!(ui.global::<Player>().get_shown(), 4.);
        assert_eq!(ui.global::<Library>().get_queue().row_count(), 1);
        assert_eq!(ui.global::<Library>().get_title(), "Local");
        apply(&ui, Event::Player(Arc::default()), &bridge);
        assert_eq!(ui.global::<Player>().get_track_id(), "");
        assert_eq!(ui.global::<Player>().get_shown(), 0.);
    }
    #[test]
    fn padded_window_is_clamped_to_the_list() {
        assert_eq!(Bridge::padded((0, 10), 100), 0..18);
        assert_eq!(Bridge::padded((50, 10), 100), 42..68);
        assert_eq!(Bridge::padded((95, 10), 100), 87..100);
        assert_eq!(Bridge::padded((0, 10), 0), 0..0);
    }
}
