// SPDX-License-Identifier: GPL-3.0-or-later
//! Desktop-facing commands and incremental events. No UI types cross this boundary.
use eagle_core::{Album, Artist, Playlist, Provider, Quality, Track, TrackId};
pub use eagle_core::{Availability, Token};
use eagle_engine::{Config, Engine, PositionAnchor, State};
use eagle_store::{Cache, Store};
use std::{
    cell::RefCell,
    collections::{HashSet, VecDeque},
    path::PathBuf,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
/// Above this many inserts/removals a list change is sent as one reset.
const RESET_THRESHOLD: usize = 32;
const LIKED_ALBUMS: &str = "liked_albums:yandex";
const LIKED_ARTISTS: &str = "liked_artists:yandex";

fn names(artists: &[Artist]) -> String {
    artists
        .iter()
        .map(|a| a.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}
fn album_card(album: &Album) -> Card {
    Card {
        id: album.id.clone(),
        key: format!("a:{}", album.id),
        title: album.title.clone(),
        subtitle: match (album.year, names(&album.artists)) {
            (Some(year), artists) if !artists.is_empty() => format!("{year} · {artists}"),
            (Some(year), _) => year.to_string(),
            (None, artists) => artists,
        },
        cover: album.cover.clone(),
        round: false,
    }
}
fn artist_card(artist: &Artist) -> Card {
    Card {
        id: artist.id.clone(),
        key: format!("r:{}", artist.id),
        title: artist.name.clone(),
        subtitle: String::new(),
        cover: artist.cover.clone(),
        round: true,
    }
}
fn playlist_card(playlist: &Playlist) -> Card {
    Card {
        id: playlist.id.clone(),
        key: format!("p:{}", playlist.id),
        title: playlist.title.clone(),
        subtitle: String::new(),
        cover: playlist.cover.clone(),
        round: false,
    }
}
fn header_cover(id: &TrackId, kind: &str, uri: Option<String>) -> Option<crate::covers::Job> {
    uri.map(|uri| crate::covers::Job {
        key: format!("h:{id}"),
        stem: format!("{kind}_{}", id.to_string().replace(':', "_")),
        uri: Some(uri),
        size: 600,
    })
}
fn card_job(card: &Card) -> crate::covers::Job {
    let kind = if card.round {
        "artist"
    } else if card.key.starts_with("p:") {
        "playlist"
    } else {
        "album"
    };
    crate::covers::Job {
        key: card.key.clone(),
        stem: format!("{kind}_{}", card.id.to_string().replace(':', "_")),
        uri: card.cover.clone(),
        size: 300,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Library,
    Search,
    Album,
    Playlist,
    Login,
    Settings,
    /// Liked albums grid.
    Albums,
    /// Liked artists grid.
    Artists,
    /// One artist: popular tracks and discography.
    Artist,
}

pub enum Command {
    Open(View),
    Library,
    /// Checks a stored token and, if it is valid, syncs in the background.
    Restore,
    SyncLibrary,
    SyncCollections,
    Collections,
    Search(String),
    SearchArtists(String),
    SearchAlbums(String),
    SearchPlaylists(String),
    CreatePlaylist(String),
    DeletePlaylist(TrackId),
    Login(Token),
    Album(TrackId),
    Playlist(TrackId),
    Artist(TrackId),
    Albums,
    Artists,
    LikeAlbum(TrackId),
    LikeArtist(TrackId),
    /// Artwork for on-screen items, by UI key (`t:` track, `a:` album, `r:` artist).
    Thumbnails(Vec<String>),
    Play(TrackId),
    PlayAll(bool),
    TogglePlay,
    Pause,
    Resume,
    Stop,
    Next,
    Previous,
    Seek(Duration),
    Volume(f32),
    Like(TrackId),
    ToggleShuffle,
    SetShuffle(bool),
    CycleRepeat,
    Shutdown,
}
#[derive(Clone, Debug)]
pub struct LibraryItem {
    pub track: Arc<Track>,
    pub liked: bool,
}
#[derive(Clone, Debug)]
pub enum LibraryDelta {
    Insert { index: usize, item: LibraryItem },
    Update { index: usize, item: LibraryItem },
    Remove { index: usize },
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}
#[derive(Debug)]
pub struct PlayerState {
    pub track: Option<Arc<Track>>,
    pub status: State,
    pub volume: f32,
    pub liked: bool,
    pub shuffle: bool,
    pub repeat: Repeat,
}
impl Default for PlayerState {
    fn default() -> Self {
        Self {
            track: None,
            status: State::Idle,
            volume: 1.,
            liked: false,
            shuffle: false,
            repeat: Repeat::Off,
        }
    }
}
#[derive(Clone, Copy)]
pub enum CoverTarget {
    Player,
    Collection,
}
/// Album or artist tile in a grid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Card {
    pub id: TrackId,
    pub key: String,
    pub title: String,
    pub subtitle: String,
    pub cover: Option<String>,
    pub round: bool,
}
/// Extra header data for album and artist screens.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Header {
    pub id: String,
    /// Artist line under an album title.
    pub subtitle: String,
    pub subtitle_id: String,
    pub liked: bool,
    pub likeable: bool,
    pub deletable: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Collection {
    pub id: TrackId,
    pub title: String,
}
pub enum Event {
    Collections(Vec<Collection>),
    PlaylistCreated(TrackId),
    PlaylistDeleted(TrackId),
    PlaylistMutationBusy(bool),
    PlaylistMutationFailed(String),
    Collection {
        title: String,
        length: Duration,
    },
    Queue(Vec<Arc<Track>>),
    View(View),
    Busy(bool),
    Player(Arc<PlayerState>),
    Position(PositionAnchor),
    Library(Vec<LibraryDelta>),
    /// Whole-list replacement: cheaper for the UI than thousands of row deltas.
    LibraryReset(Vec<LibraryItem>),
    Error(String),
    Account,
    Cards(Vec<Card>),
    Header(Header),
    /// Album or artist like state, by id.
    CollectionLiked(TrackId, bool),
    /// Decoded artwork for the item with this UI key.
    Thumbnail(String, Arc<eagle_palette::Cover>),
    Cover(CoverTarget, TrackId, Arc<eagle_palette::Cover>),
}

/// Interpolate from the audible anchor, including device latency, with no background timer.
pub fn position_now(anchor: &PositionAnchor, now: Instant) -> Duration {
    if anchor.state != State::Playing {
        return anchor.position;
    }
    if now >= anchor.audible_at {
        anchor
            .position
            .saturating_add(now.duration_since(anchor.audible_at))
    } else {
        anchor
            .position
            .saturating_sub(anchor.audible_at.duration_since(now))
    }
}

fn initial_queue(rows: &[LibraryItem], selected: usize) -> Result<(Vec<Arc<Track>>, usize)> {
    if rows
        .get(selected)
        .is_none_or(|row| row.track.availability != Availability::Available)
    {
        return Err("трек недоступен".into());
    }
    let current = rows[..selected]
        .iter()
        .filter(|row| row.track.availability == Availability::Available)
        .count();
    let mut seen = HashSet::new();
    let queue = rows
        .iter()
        .filter(|row| row.track.availability == Availability::Available)
        .map(|row| {
            if seen.insert(Arc::as_ptr(&row.track)) {
                row.track.clone()
            } else {
                Arc::new((*row.track).clone())
            }
        })
        .collect();
    Ok((queue, current))
}

fn upcoming(queue: &[Arc<Track>], current: usize, repeat: Repeat) -> Vec<Arc<Track>> {
    if current >= queue.len() {
        return Vec::new();
    }
    let remaining = queue.iter().skip(current + 1);
    if repeat == Repeat::All {
        remaining
            .chain(queue.iter().take(current))
            .take(4)
            .cloned()
            .collect()
    } else {
        remaining.take(4).cloned().collect()
    }
}

fn playback_order(queue: &[Arc<Track>], sources: &[Arc<Track>]) -> Result<Vec<usize>> {
    let indices: std::collections::HashMap<_, _> = sources
        .iter()
        .enumerate()
        .map(|(index, source)| (Arc::as_ptr(source), index))
        .collect();
    queue
        .iter()
        .map(|track| {
            indices
                .get(&Arc::as_ptr(track))
                .copied()
                .ok_or_else(|| "трек отсутствует в очереди движка".into())
        })
        .collect()
}

fn requires_reset(rows: &[LibraryItem], incoming: &[LibraryItem]) -> bool {
    rows.len().abs_diff(incoming.len())
        + rows
            .iter()
            .zip(incoming)
            .filter(|(old, new)| old.track.id != new.track.id)
            .count()
        > RESET_THRESHOLD
}

/// Preserve row identity and scroll position when search results or likes change.
pub fn reconcile(rows: &mut Vec<LibraryItem>, incoming: Vec<LibraryItem>) -> Vec<LibraryDelta> {
    let count = incoming.len();
    let mut delta = Vec::new();
    for (index, item) in incoming.into_iter().enumerate() {
        if rows.get(index).is_some_and(|r| r.track.id == item.track.id) {
            if rows[index].liked != item.liked || rows[index].track != item.track {
                rows[index] = item.clone();
                delta.push(LibraryDelta::Update { index, item });
            }
        } else {
            rows.insert(index, item.clone());
            delta.push(LibraryDelta::Insert { index, item });
        }
    }
    while rows.len() > count {
        let index = rows.len() - 1;
        rows.pop();
        delta.push(LibraryDelta::Remove { index });
    }
    delta
}

enum Message {
    Command(Command),
    Catalog(u64, std::result::Result<CatalogReply, String>),
    Engine(u64, eagle_engine::Event),
}
#[derive(Clone)]
pub struct CommandSender(mpsc::Sender<Message>);
impl CommandSender {
    pub fn send(&self, command: Command) -> std::result::Result<(), mpsc::SendError<Command>> {
        self.0
            .send(Message::Command(command))
            .map_err(|e| match e.0 {
                Message::Command(command) => mpsc::SendError(command),
                _ => unreachable!(),
            })
    }
}
pub struct Session {
    commands: CommandSender,
    worker: Option<thread::JoinHandle<()>>,
}
impl Session {
    pub fn start(
        root: PathBuf,
        emit: impl Fn(Event) + Send + Sync + 'static,
    ) -> std::io::Result<Self> {
        let (commands, rx) = mpsc::channel();
        let tx = commands.clone();
        let emit = Arc::new(emit);
        let worker = thread::Builder::new()
            .name("eg-app".into())
            .spawn(move || {
                if let Err(e) = Worker::new(root, emit.clone(), tx).and_then(|mut w| w.run(rx)) {
                    (emit)(Event::Error(e.to_string()));
                }
            })?;
        Ok(Self {
            commands: CommandSender(commands),
            worker: Some(worker),
        })
    }
    pub fn sender(&self) -> CommandSender {
        self.commands.clone()
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
struct Worker {
    store: Arc<Store>,
    cache: Arc<Cache>,
    net: Arc<eagle_net::Client>,
    provider: Arc<dyn Provider>,
    catalog: Option<mpsc::Sender<CatalogRequest>>,
    catalog_worker: Option<thread::JoinHandle<()>>,
    engine: Option<Engine>,
    engine_events: Option<thread::JoinHandle<()>>,
    engine_generation: u64,
    tx: mpsc::Sender<Message>,
    rows: Vec<LibraryItem>,
    queue: Vec<Arc<Track>>,
    queue_likes: HashSet<TrackId>,
    queue_original: Vec<Arc<Track>>,
    current: usize,
    engine_queue: Vec<Arc<Track>>,
    view_generation: u64,
    view: View,
    playlist_id: Option<TrackId>,
    cards: Vec<Card>,
    covers: crate::covers::Covers,
    last_position: Option<PositionAnchor>,
    player: Arc<PlayerState>,
    emit: Arc<dyn Fn(Event) + Send + Sync>,
}
impl Worker {
    fn new(
        root: PathBuf,
        emit: Arc<dyn Fn(Event) + Send + Sync>,
        tx: mpsc::Sender<Message>,
    ) -> Result<Self> {
        let store = Arc::new(Store::open(&root)?);
        let cache = Arc::new(Cache::open(
            &root.join("audio"),
            2 * 1024 * 1024 * 1024,
            &store,
        )?);
        let net = Arc::new(eagle_net::Client::new()?);
        let provider: Arc<dyn Provider> = Arc::new(eagle_yandex::Yandex::new(net.clone()));
        let (catalog, requests) = mpsc::channel();
        let services = Catalog {
            covers: RefCell::new(VecDeque::new()),
            last_album: RefCell::new(None),
            last_artist: RefCell::new(None),
            root: root.clone(),
            net: net.clone(),
            store: store.clone(),
            provider: provider.clone(),
        };
        let replies = tx.clone();
        let catalog_worker = thread::Builder::new()
            .name("eg-catalog".into())
            .spawn(move || {
                services.run(requests, replies);
            })?;
        let covers = crate::covers::Covers::start(root.clone(), net.clone(), emit.clone())?;
        Ok(Self {
            store,
            cache,
            net,
            provider,
            catalog: Some(catalog),
            catalog_worker: Some(catalog_worker),
            engine: None,
            engine_events: None,
            engine_generation: 0,
            tx,
            rows: Vec::new(),
            queue: Vec::new(),
            queue_likes: HashSet::new(),
            queue_original: Vec::new(),
            current: 0,
            engine_queue: Vec::new(),
            view_generation: 0,
            view: View::Library,
            playlist_id: None,
            cards: Vec::new(),
            covers,
            last_position: None,
            player: Arc::default(),
            emit,
        })
    }
    fn run(&mut self, rx: mpsc::Receiver<Message>) -> Result<()> {
        (self.emit)(Event::Player(self.player.clone()));
        self.request(Command::Collections)?;
        self.request(Command::Library)?;
        self.background(Command::Restore)?;
        while let Ok(message) = rx.recv() {
            match message {
                Message::Command(Command::Shutdown) => break,
                Message::Command(command) => {
                    if let Err(e) = self.command(command) {
                        (self.emit)(Event::Error(e.to_string()));
                    }
                }
                Message::Engine(generation, event) if generation == self.engine_generation => {
                    self.engine_event(event)
                }
                Message::Engine(_, _) => {}
                Message::Catalog(generation, reply) => {
                    if generation != 0
                        && generation != self.view_generation
                        && !matches!(
                            &reply,
                            Ok(CatalogReply::PlaylistChanged(..) | CatalogReply::PlaylistFailed(_))
                        )
                    {
                        continue;
                    }
                    if generation != 0
                        && generation == self.view_generation
                        && !matches!(
                            &reply,
                            Ok(CatalogReply::PlaylistChanged(..) | CatalogReply::PlaylistFailed(_))
                        )
                    {
                        (self.emit)(Event::Busy(false));
                    }
                    match reply {
                        Ok(CatalogReply::Tracks(items, title)) => {
                            let length = Duration::from_millis(
                                items.iter().map(|r| r.track.duration_ms).sum(),
                            );
                            if let Some(first) = items.first()
                                && let Some(catalog) = &self.catalog
                            {
                                let _ = catalog.send(CatalogRequest::Cover(
                                    first.track.clone(),
                                    CoverTarget::Collection,
                                    self.view_generation,
                                ));
                            }
                            self.publish(items);
                            (self.emit)(Event::Collection { title, length });
                        }
                        Ok(CatalogReply::Page {
                            items,
                            title,
                            header,
                            cards,
                            cover,
                        }) => {
                            let length = Duration::from_millis(
                                items.iter().map(|r| r.track.duration_ms).sum(),
                            );
                            match cover {
                                Some(job) => self.covers.request(vec![job]),
                                None => {
                                    if let Some(first) = items.first()
                                        && let Some(catalog) = &self.catalog
                                    {
                                        let _ = catalog.send(CatalogRequest::Cover(
                                            first.track.clone(),
                                            CoverTarget::Collection,
                                            self.view_generation,
                                        ));
                                    }
                                }
                            }
                            self.publish(items);
                            self.cards = cards.clone();
                            (self.emit)(Event::Collection { title, length });
                            (self.emit)(Event::Header(header));
                            (self.emit)(Event::Cards(cards));
                        }
                        Ok(CatalogReply::Cards(cards)) => {
                            self.publish(Vec::new());
                            self.cards = cards.clone();
                            (self.emit)(Event::Cards(cards));
                        }
                        Ok(CatalogReply::CollectionLiked(id, liked)) => {
                            (self.emit)(Event::CollectionLiked(id, liked))
                        }
                        Ok(CatalogReply::LikeFailed(id, liked, message)) => {
                            self.apply_liked(id, liked);
                            (self.emit)(Event::Error(message));
                        }
                        Ok(CatalogReply::Liked(id, liked)) => self.apply_liked(id, liked),
                        Ok(CatalogReply::PlaylistChanged(collections, created, deleted)) => {
                            self.playlist_changed(generation, collections, created, deleted)?;
                        }
                        Ok(CatalogReply::PlaylistFailed(message)) => {
                            (self.emit)(Event::PlaylistMutationBusy(false));
                            (self.emit)(Event::PlaylistMutationFailed(message));
                        }
                        Ok(CatalogReply::Collections(collections)) => {
                            (self.emit)(Event::Collections(collections))
                        }
                        Ok(CatalogReply::Unchanged) => {}
                        Ok(CatalogReply::Cover(target, id, cover)) => {
                            (self.emit)(Event::Cover(target, id, cover))
                        }
                        Ok(CatalogReply::Account) => {
                            (self.emit)(Event::Account);
                            self.background(Command::SyncCollections)?;
                            self.background(Command::SyncLibrary)?;
                        }
                        Ok(CatalogReply::Synced) => {
                            if self.view == View::Library {
                                // View-bound, so a reply racing a navigation is discarded.
                                self.request(Command::Library)?;
                            }
                        }
                        Err(message) if generation == 0 => {
                            // Background sync failures (offline, expired token) keep the cached view.
                            log::warn!(error = message.as_str(); "background catalog request failed");
                        }
                        Err(message) => (self.emit)(Event::Error(message)),
                    }
                }
            }
        }
        self.stop_engine();
        self.catalog.take();
        if let Some(worker) = self.catalog_worker.take() {
            let _ = worker.join();
        }
        Ok(())
    }
    fn playlist_changed(
        &mut self,
        generation: u64,
        collections: Vec<Collection>,
        created: Option<Playlist>,
        deleted: Option<TrackId>,
    ) -> Result<()> {
        (self.emit)(Event::PlaylistMutationBusy(false));
        (self.emit)(Event::Collections(collections));
        if let Some(playlist) = created {
            (self.emit)(Event::PlaylistCreated(playlist.id.clone()));
            if generation == self.view_generation {
                self.playlist_id = Some(playlist.id.clone());
                self.view = View::Playlist;
                self.view_generation += 1;
                (self.emit)(Event::View(View::Playlist));
                self.publish(Vec::new());
                self.cards.clear();
                (self.emit)(Event::Collection {
                    title: playlist.title,
                    length: Duration::ZERO,
                });
                (self.emit)(Event::Header(Header {
                    id: playlist.id.to_string(),
                    deletable: true,
                    ..Header::default()
                }));
            }
        } else if let Some(id) = deleted {
            (self.emit)(Event::PlaylistDeleted(id.clone()));
            if generation == self.view_generation
                && self.view == View::Playlist
                && self.playlist_id.as_ref() == Some(&id)
            {
                self.command(Command::Open(View::Library))?;
            }
        }
        Ok(())
    }
    fn stop_engine(&mut self) {
        self.engine_generation = self.engine_generation.wrapping_add(1);
        self.engine.take();
        if let Some(worker) = self.engine_events.take() {
            let _ = worker.join();
        }
    }
    /// Authoritative like state for a track: queue, player and the visible row.
    fn apply_liked(&mut self, id: TrackId, liked: bool) {
        if liked {
            self.queue_likes.insert(id.clone());
        } else {
            self.queue_likes.remove(&id);
        }
        if self
            .player
            .track
            .as_ref()
            .is_some_and(|track| track.id == id)
        {
            Arc::make_mut(&mut self.player).liked = liked;
            (self.emit)(Event::Player(self.player.clone()));
        }
        if let Some(index) = self.rows.iter().position(|r| r.track.id == id) {
            self.rows[index].liked = liked;
            (self.emit)(Event::Library(vec![LibraryDelta::Update {
                index,
                item: self.rows[index].clone(),
            }]));
        }
    }
    /// Maps UI keys to cover jobs using what is currently listed.
    fn thumbnails(&self, keys: Vec<String>) {
        let mut jobs = Vec::new();
        for key in keys {
            if let Some(id) = key.strip_prefix("t:") {
                let Ok(id) = id.parse::<TrackId>() else {
                    continue;
                };
                let track = self
                    .rows
                    .iter()
                    .map(|r| &r.track)
                    .chain(self.queue.iter())
                    .find(|t| t.id == id);
                if let Some(track) = track {
                    jobs.push(crate::covers::Job {
                        stem: track.id.to_string().replace(':', "_"),
                        uri: track.cover.clone(),
                        size: 64,
                        key,
                    });
                }
            } else if let Some(card) = self.cards.iter().find(|c| c.key == key) {
                jobs.push(card_job(card));
            }
        }
        self.covers.request(jobs);
    }
    fn publish(&mut self, incoming: Vec<LibraryItem>) {
        if requires_reset(&self.rows, &incoming) {
            self.rows = incoming;
            (self.emit)(Event::LibraryReset(self.rows.clone()));
        } else {
            let delta = reconcile(&mut self.rows, incoming);
            if !delta.is_empty() {
                (self.emit)(Event::Library(delta));
            }
        }
    }
    /// Catalog work that is not tied to the visible screen and never shows a spinner or error.
    fn background(&mut self, command: Command) -> Result<()> {
        self.catalog
            .as_ref()
            .ok_or("каталог остановлен")?
            .send(CatalogRequest::Command(0, command))
            .map_err(|_| "каталог остановлен")?;
        Ok(())
    }
    fn request(&mut self, command: Command) -> Result<()> {
        let generation = if matches!(
            &command,
            Command::Library
                | Command::Collections
                | Command::Search(_)
                | Command::SearchArtists(_)
                | Command::SearchAlbums(_)
                | Command::SearchPlaylists(_)
                | Command::Album(_)
                | Command::Playlist(_)
                | Command::Artist(_)
                | Command::Albums
                | Command::Artists
        ) {
            self.view_generation += 1;
            (self.emit)(Event::Busy(true));
            self.view_generation
        } else {
            0
        };
        self.catalog
            .as_ref()
            .ok_or("каталог остановлен")?
            .send(CatalogRequest::Command(generation, command))
            .map_err(|_| "каталог остановлен")?;
        Ok(())
    }
    fn command(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Open(view) => {
                self.playlist_id = None;
                self.view = view;
                (self.emit)(Event::View(view));
                self.view_generation += 1;
                (self.emit)(Event::Busy(false));
                self.cards.clear();
                match view {
                    View::Library => self.request(Command::Library)?,
                    View::Albums => self.request(Command::Albums)?,
                    View::Artists => self.request(Command::Artists)?,
                    View::Search | View::Album | View::Playlist | View::Artist => {
                        self.publish(Vec::new())
                    }
                    View::Login | View::Settings => {}
                }
            }
            command @ (Command::Restore
            | Command::SyncLibrary
            | Command::SyncCollections
            | Command::LikeAlbum(_)
            | Command::LikeArtist(_)) => self.background(command)?,
            Command::Thumbnails(keys) => self.thumbnails(keys),
            command @ (Command::Albums | Command::Artists) => self.request(command)?,
            Command::Artist(id) => {
                self.view = View::Artist;
                (self.emit)(Event::View(View::Artist));
                self.publish(Vec::new());
                self.cards.clear();
                self.request(Command::Artist(id))?;
            }
            command @ (Command::Search(_)
            | Command::SearchArtists(_)
            | Command::SearchAlbums(_)
            | Command::SearchPlaylists(_)) => {
                self.publish(Vec::new());
                self.cards.clear();
                (self.emit)(Event::Cards(Vec::new()));
                let query = match &command {
                    Command::Search(query)
                    | Command::SearchArtists(query)
                    | Command::SearchAlbums(query)
                    | Command::SearchPlaylists(query) => query,
                    _ => unreachable!(),
                };
                if query.trim().is_empty() {
                    self.view_generation += 1;
                    (self.emit)(Event::Busy(false));
                } else {
                    self.request(command)?;
                }
            }
            command @ (Command::Library
            | Command::Collections
            | Command::Login(_)
            | Command::Like(_)) => self.request(command)?,
            Command::Album(id) => {
                self.view = View::Album;
                (self.emit)(Event::View(View::Album));
                self.publish(Vec::new());
                self.cards.clear();
                self.request(Command::Album(id))?;
            }
            command @ (Command::CreatePlaylist(_) | Command::DeletePlaylist(_)) => {
                (self.emit)(Event::PlaylistMutationBusy(true));
                if self.catalog.as_ref().is_none_or(|catalog| {
                    catalog
                        .send(CatalogRequest::Command(self.view_generation, command))
                        .is_err()
                }) {
                    (self.emit)(Event::PlaylistMutationBusy(false));
                    (self.emit)(Event::PlaylistMutationFailed("каталог остановлен".into()));
                }
            }
            Command::Playlist(id) => {
                self.playlist_id = Some(id.clone());
                self.view = View::Playlist;
                (self.emit)(Event::View(View::Playlist));
                self.publish(Vec::new());
                self.request(Command::Playlist(id))?;
            }
            Command::PlayAll(shuffle) => {
                Arc::make_mut(&mut self.player).shuffle = shuffle;
                let track = self
                    .rows
                    .iter()
                    .find(|r| r.track.availability == Availability::Available)
                    .map(|r| r.track.id.clone());
                if let Some(id) = track {
                    self.command(Command::Play(id))?;
                }
            }
            Command::Play(id) => {
                let index = self
                    .rows
                    .iter()
                    .position(|r| r.track.id == id)
                    .ok_or("трек не найден в текущем списке")?;
                let (queue, selected) = initial_queue(&self.rows, index)?;
                self.queue = queue;
                self.queue_original = self.queue.clone();
                let track = self.queue[selected].clone();
                self.queue_likes = self
                    .rows
                    .iter()
                    .filter(|r| r.liked)
                    .map(|r| r.track.id.clone())
                    .collect();
                if self.player.shuffle {
                    use std::hash::{BuildHasher, RandomState};
                    let selected = self
                        .queue
                        .iter()
                        .position(|t| Arc::ptr_eq(t, &track))
                        .ok_or("трек недоступен")?;
                    self.queue.remove(selected);
                    let random = RandomState::new();
                    self.queue.sort_by_cached_key(|t| random.hash_one(&t.id));
                    self.queue.insert(0, track);
                    self.current = 0;
                } else {
                    self.current = self
                        .queue
                        .iter()
                        .position(|t| t.id == track.id)
                        .ok_or("трек недоступен")?;
                }
                self.play_current()?;
            }
            Command::Next => {
                if let Some(engine) = &self.engine {
                    engine.next()?;
                }
            }
            Command::Previous => {
                let paused = self.player.status == State::Paused;
                self.current = self.current.saturating_sub(1);
                self.play_current()?;
                if paused && let Some(engine) = &self.engine {
                    engine.pause()?;
                }
            }
            Command::Pause => {
                if let Some(engine) = &self.engine {
                    engine.pause()?;
                }
            }
            Command::Resume => {
                if self.player.status == State::Idle {
                    self.play_current()?;
                } else if let Some(engine) = &self.engine {
                    engine.resume()?;
                } else {
                    self.play_current()?;
                }
            }
            Command::Stop => {
                self.stop_engine();
                Arc::make_mut(&mut self.player).status = State::Idle;
                (self.emit)(Event::Player(self.player.clone()));
            }
            Command::TogglePlay => {
                if self.player.status == State::Idle {
                    self.play_current()?;
                } else if let Some(engine) = &self.engine {
                    if self.player.status == State::Paused {
                        engine.resume()?;
                    } else {
                        engine.pause()?;
                    }
                }
            }
            Command::Seek(to) => {
                if let Some(engine) = &self.engine {
                    engine.seek(to)?;
                }
            }
            Command::Volume(v) => {
                if !v.is_finite() {
                    return Err("неверная громкость".into());
                }
                if let Some(engine) = &self.engine {
                    engine.volume(v)?;
                }
                Arc::make_mut(&mut self.player).volume = v.clamp(0., 1.);
                (self.emit)(Event::Player(self.player.clone()));
            }
            Command::SetShuffle(shuffle) => {
                if self.player.shuffle != shuffle {
                    self.command(Command::ToggleShuffle)?;
                }
            }
            Command::ToggleShuffle => {
                Arc::make_mut(&mut self.player).shuffle ^= true;
                if self.player.shuffle && self.queue.len() > self.current + 1 {
                    use std::hash::{BuildHasher, RandomState};
                    let random = RandomState::new();
                    self.queue[self.current + 1..].sort_by_cached_key(|t| random.hash_one(&t.id));
                }
                if !self.player.shuffle && self.queue.len() > self.current + 1 {
                    let order: std::collections::HashMap<_, _> = self
                        .queue_original
                        .iter()
                        .enumerate()
                        .map(|(i, t)| (Arc::as_ptr(t), i))
                        .collect();
                    self.queue[self.current + 1..]
                        .sort_by_key(|t| order.get(&Arc::as_ptr(t)).copied().unwrap_or(usize::MAX));
                }
                self.configure_playback()?;
                self.emit_queue();
                (self.emit)(Event::Player(self.player.clone()));
            }
            Command::CycleRepeat => {
                let state = Arc::make_mut(&mut self.player);
                state.repeat = match state.repeat {
                    Repeat::Off => Repeat::All,
                    Repeat::All => Repeat::One,
                    Repeat::One => Repeat::Off,
                };
                self.configure_playback()?;
                self.emit_queue();
                (self.emit)(Event::Player(self.player.clone()));
            }
            Command::Shutdown => {}
        }
        Ok(())
    }
    fn configure_playback(&self) -> Result<()> {
        if let Some(engine) = &self.engine {
            let order = playback_order(&self.queue, &self.engine_queue)?;
            let repeat = match self.player.repeat {
                Repeat::Off => eagle_engine::Repeat::Off,
                Repeat::All => eagle_engine::Repeat::All,
                Repeat::One => eagle_engine::Repeat::One,
            };
            engine.configure_playback(order, repeat)?;
        }
        Ok(())
    }
    fn emit_queue(&self) {
        (self.emit)(Event::Queue(upcoming(
            &self.queue,
            self.current,
            self.player.repeat,
        )));
    }
    fn play_current(&mut self) -> Result<()> {
        let Some(track) = self.queue.get(self.current).cloned() else {
            return Ok(());
        };
        log::info!(target: "eagle_app", provider = track.id.provider(), track_id = track.id.local(); "playback");
        self.stop_engine();
        self.last_position = None;
        let mut engine = Engine::new(Config::default())?;
        engine.volume(self.player.volume)?;
        self.engine_queue = self.queue[self.current..]
            .iter()
            .chain(&self.queue[..self.current])
            .cloned()
            .collect();
        for item in &self.engine_queue {
            let provider = self.provider.clone();
            let id = item.id.clone();
            let resolve = Arc::new(move || {
                let stream = provider
                    .resolve_stream(&id, Quality::High)
                    .map_err(|_| eagle_net::Error::Source("не удалось получить аудиоссылку"))?;
                if !matches!(stream.encryption, eagle_core::Encryption::None) {
                    return Err(eagle_net::Error::Source(
                        "шифрованный поток не поддерживается",
                    ));
                }
                Ok(eagle_net::growing::Location {
                    url: stream.url,
                    expires_at: Some(stream.expires_at),
                })
            });
            engine.enqueue(crate::playback::streamed(
                self.store.clone(),
                self.cache.clone(),
                self.net.clone(),
                item.id.clone(),
                Quality::High,
                resolve,
            )?)?;
        }
        let events = engine.take_events();
        self.engine_generation += 1;
        let generation = self.engine_generation;
        let tx = self.tx.clone();
        self.engine_events = Some(thread::Builder::new().name("eg-engine-ev".into()).spawn(
            move || {
                for event in events {
                    if tx.send(Message::Engine(generation, event)).is_err() {
                        break;
                    }
                }
            },
        )?);
        self.engine = Some(engine);
        self.configure_playback()?;
        let state = Arc::make_mut(&mut self.player);
        state.liked = self.queue_likes.contains(&track.id);
        state.track = Some(track.clone());
        state.status = State::Loading;
        if let Some(catalog) = &self.catalog {
            let _ = catalog.send(CatalogRequest::Cover(track, CoverTarget::Player, 0));
        }
        (self.emit)(Event::Player(self.player.clone()));
        self.emit_queue();
        Ok(())
    }
    fn engine_event(&mut self, event: eagle_engine::Event) {
        match event {
            eagle_engine::Event::TrackChanged(index) => {
                let Some(track) = self.engine_queue.get(index as usize) else {
                    return;
                };
                let Some(current) = self.queue.iter().position(|item| Arc::ptr_eq(item, track))
                else {
                    return;
                };
                self.current = current;
                self.last_position = None;
                self.emit_queue();
                if let Some(track) = self.queue.get(self.current).cloned() {
                    let state = Arc::make_mut(&mut self.player);
                    state.liked = self.queue_likes.contains(&track.id);
                    state.track = Some(track.clone());
                    if let Some(catalog) = &self.catalog {
                        let _ = catalog.send(CatalogRequest::Cover(track, CoverTarget::Player, 0));
                    }
                    (self.emit)(Event::Player(self.player.clone()));
                }
            }
            eagle_engine::Event::StateChanged(status) => {
                Arc::make_mut(&mut self.player).status = status;
                (self.emit)(Event::Player(self.player.clone()));
            }
            eagle_engine::Event::Position(anchor) => {
                self.last_position = Some(anchor.clone());
                (self.emit)(Event::Position(anchor));
            }
            eagle_engine::Event::Error { message, .. } => (self.emit)(Event::Error(message)),
            _ => {}
        }
    }
}

#[derive(Default)]
struct SyncState {
    step: u8,
    cursor: Option<String>,
    ids: Vec<String>,
}

enum CatalogRequest {
    Command(u64, Command),
    Cover(Arc<Track>, CoverTarget, u64),
}
enum CatalogReply {
    Collections(Vec<Collection>),
    PlaylistChanged(Vec<Collection>, Option<Playlist>, Option<TrackId>),
    PlaylistFailed(String),
    Synced,
    Cards(Vec<Card>),
    Page {
        items: Vec<LibraryItem>,
        title: String,
        header: Header,
        cards: Vec<Card>,
        cover: Option<crate::covers::Job>,
    },
    LikeFailed(TrackId, bool, String),
    CollectionLiked(TrackId, bool),
    Unchanged,
    Cover(CoverTarget, TrackId, Arc<eagle_palette::Cover>),
    Tracks(Vec<LibraryItem>, String),
    Liked(TrackId, bool),
    Account,
}
struct Catalog {
    covers: RefCell<VecDeque<(TrackId, Arc<eagle_palette::Cover>)>>,
    /// Last opened album/artist, so a like from its header can be listed without a refetch.
    last_album: RefCell<Option<Album>>,
    last_artist: RefCell<Option<Artist>>,
    root: PathBuf,
    net: Arc<eagle_net::Client>,
    store: Arc<Store>,
    provider: Arc<dyn Provider>,
}
impl Catalog {
    fn run(&self, requests: mpsc::Receiver<CatalogRequest>, replies: mpsc::Sender<Message>) {
        let mut sync: Option<SyncState> = None;
        loop {
            let request = if sync.is_some() {
                match requests.try_recv() {
                    Ok(request) => Some(request),
                    Err(mpsc::TryRecvError::Empty) => None,
                    Err(mpsc::TryRecvError::Disconnected) => break,
                }
            } else {
                match requests.recv() {
                    Ok(request) => Some(request),
                    Err(_) => break,
                }
            };
            if let Some(request) = request {
                match request {
                    CatalogRequest::Command(_, Command::SyncCollections | Command::SyncLibrary) => {
                        if sync.is_none() {
                            sync = Some(SyncState::default());
                        }
                    }
                    request => {
                        if matches!(&request, CatalogRequest::Command(_, Command::Login(_))) {
                            sync = None;
                        } else if sync.is_some()
                            && matches!(&request, CatalogRequest::Command(_, Command::Like(_)))
                        {
                            sync = Some(SyncState::default());
                        }
                        let generation = match &request {
                            CatalogRequest::Command(generation, _) => *generation,
                            CatalogRequest::Cover(_, _, generation) => *generation,
                        };
                        let mutation = matches!(
                            &request,
                            CatalogRequest::Command(
                                _,
                                Command::CreatePlaylist(_) | Command::DeletePlaylist(_)
                            )
                        );
                        let reply = self.request(request).map_err(|e| e.to_string());
                        let reply = if mutation {
                            Ok(reply.unwrap_or_else(CatalogReply::PlaylistFailed))
                        } else {
                            reply
                        };
                        if replies.send(Message::Catalog(generation, reply)).is_err() {
                            break;
                        }
                    }
                }
            } else if let Some(state) = &mut sync {
                match self.sync_step(state) {
                    Ok((reply, done)) => {
                        if let Some(reply) = reply
                            && replies.send(Message::Catalog(0, Ok(reply))).is_err()
                        {
                            break;
                        }
                        if done {
                            sync = None;
                        }
                    }
                    Err(error) => {
                        log::warn!(error = error.to_string().as_str(); "library sync failed");
                        sync = None;
                    }
                }
            }
        }
    }

    fn sync_step(&self, state: &mut SyncState) -> Result<(Option<CatalogReply>, bool)> {
        match state.step {
            0 => {
                crate::music::sync_playlists(&self.store, self.provider.as_ref())?;
                state.step = 1;
                Ok((Some(self.command(Command::Collections)?), false))
            }
            1 => {
                let page = self.provider.likes(state.cursor.as_deref())?;
                self.store.put_tracks(&page.items)?;
                state
                    .ids
                    .extend(page.items.iter().map(|track| track.id.to_string()));
                state.cursor = page.next;
                if state.cursor.is_none() {
                    state.step = 2;
                }
                Ok((None, false))
            }
            2 => {
                self.store.replace_likes(std::mem::take(&mut state.ids))?;
                state.step = 3;
                Ok((Some(CatalogReply::Synced), false))
            }
            3 => {
                match self.provider.liked_albums() {
                    Ok(albums) => self
                        .store
                        .put_setting(LIKED_ALBUMS, serde_json::to_string(&albums)?)?,
                    Err(error) => {
                        log::warn!(error = error.to_string().as_str(); "liked albums sync failed")
                    }
                }
                state.step = 4;
                Ok((None, false))
            }
            _ => {
                match self.provider.liked_artists() {
                    Ok(artists) => self
                        .store
                        .put_setting(LIKED_ARTISTS, serde_json::to_string(&artists)?)?,
                    Err(error) => {
                        log::warn!(error = error.to_string().as_str(); "liked artists sync failed")
                    }
                }
                Ok((None, true))
            }
        }
    }

    fn request(&self, request: CatalogRequest) -> Result<CatalogReply> {
        match request {
            CatalogRequest::Command(_, command) => self.command(command),
            CatalogRequest::Cover(track, target, _) => {
                {
                    let mut covers = self.covers.borrow_mut();
                    if let Some(index) = covers.iter().position(|(id, _)| id == &track.id) {
                        let entry = covers.remove(index).ok_or("обложка удалена из кэша")?;
                        let cover = entry.1.clone();
                        covers.push_front(entry);
                        return Ok(CatalogReply::Cover(target, track.id.clone(), cover));
                    }
                }
                let name = track.id.to_string().replace(':', "_");
                let path = self.root.join("covers").join(format!("{name}-600.png"));
                if !path.exists()
                    && (track.cover.is_none()
                        || crate::music::metadata(&self.store, &self.net, &self.root, &track)
                            .is_err())
                {
                    return Ok(CatalogReply::Unchanged);
                }
                let cover = Arc::new(eagle_palette::decode_cover(&std::fs::read(path)?, 600)?);
                {
                    let mut covers = self.covers.borrow_mut();
                    covers.push_front((track.id.clone(), cover.clone()));
                    if covers.len() > 3 {
                        covers.pop_back();
                    }
                }
                Ok(CatalogReply::Cover(target, track.id.clone(), cover))
            }
        }
    }

    fn library(&self) -> Result<CatalogReply> {
        let rows = {
            let c = self.store.read()?;
            // The library is the liked collection, newest like first.
            let mut stmt = c.prepare(
                "SELECT tracks.json FROM likes JOIN tracks ON tracks.id=likes.id ORDER BY likes.rowid DESC",
            )?;
            stmt.query_map([], |r| r.get::<_, String>(0))?
                .map(|json| {
                    Ok(LibraryItem {
                        track: Arc::new(serde_json::from_str(&json?)?),
                        liked: true,
                    })
                })
                .collect::<Result<Vec<_>>>()?
        };
        Ok(CatalogReply::Tracks(rows, String::new()))
    }
    fn liked_albums(&self) -> Result<Vec<Album>> {
        Ok(match self.store.setting(LIKED_ALBUMS)? {
            Some(json) => serde_json::from_str(&json)?,
            None => Vec::new(),
        })
    }
    fn liked_artists(&self) -> Result<Vec<Artist>> {
        Ok(match self.store.setting(LIKED_ARTISTS)? {
            Some(json) => serde_json::from_str(&json)?,
            None => Vec::new(),
        })
    }
    fn page(
        &self,
        tracks: Vec<Track>,
        title: String,
        header: Header,
        cards: Vec<Card>,
        cover: Option<crate::covers::Job>,
    ) -> Result<CatalogReply> {
        let CatalogReply::Tracks(items, title) = self.tracks(tracks, title)? else {
            return Err("неожиданный ответ каталога".into());
        };
        Ok(CatalogReply::Page {
            items,
            title,
            header,
            cards,
            cover,
        })
    }
    fn tracks(&self, tracks: Vec<Track>, title: String) -> Result<CatalogReply> {
        self.store.put_tracks(&tracks)?;
        let liked = {
            let c = self.store.read()?;
            let mut stmt = c.prepare("SELECT id FROM likes")?;
            stmt.query_map([], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<HashSet<_>, _>>()?
        };
        let items = tracks
            .into_iter()
            .map(|track| LibraryItem {
                liked: liked.contains(&track.id.to_string()),
                track: Arc::new(track),
            })
            .collect();
        Ok(CatalogReply::Tracks(items, title))
    }
    fn command(&self, command: Command) -> Result<CatalogReply> {
        match command {
            Command::Collections => {
                let c = self.store.read()?;
                let mut stmt = c.prepare("SELECT json FROM playlists ORDER BY rowid")?;
                let playlists = stmt
                    .query_map([], |r| r.get::<_, String>(0))?
                    .map(|json| {
                        let p: eagle_core::Playlist = serde_json::from_str(&json?)?;
                        Ok(Collection {
                            id: p.id,
                            title: p.title,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(CatalogReply::Collections(playlists))
            }
            Command::Library => self.library(),
            Command::Restore => Ok(match self.provider.account() {
                Ok(_) => CatalogReply::Account,
                Err(_) => CatalogReply::Unchanged,
            }),
            Command::SyncCollections => {
                crate::music::sync_playlists(&self.store, self.provider.as_ref())?;
                self.command(Command::Collections)
            }
            Command::SyncLibrary => {
                crate::music::sync_likes(&self.store, self.provider.as_ref())?;
                // Album and artist likes are a bonus: their failure must not hide liked tracks.
                match self.provider.liked_albums() {
                    Ok(albums) => self
                        .store
                        .put_setting(LIKED_ALBUMS, serde_json::to_string(&albums)?)?,
                    Err(error) => {
                        log::warn!(error = error.to_string().as_str(); "liked albums sync failed")
                    }
                }
                match self.provider.liked_artists() {
                    Ok(artists) => self
                        .store
                        .put_setting(LIKED_ARTISTS, serde_json::to_string(&artists)?)?,
                    Err(error) => {
                        log::warn!(error = error.to_string().as_str(); "liked artists sync failed")
                    }
                }
                Ok(CatalogReply::Synced)
            }
            Command::Search(query) => {
                self.tracks(self.provider.search(&query, 0)?.items, String::new())
            }
            Command::SearchArtists(query) => Ok(CatalogReply::Cards(
                self.provider
                    .search_artists(&query, 0)?
                    .items
                    .iter()
                    .map(artist_card)
                    .collect(),
            )),
            Command::SearchAlbums(query) => Ok(CatalogReply::Cards(
                self.provider
                    .search_albums(&query, 0)?
                    .items
                    .iter()
                    .map(album_card)
                    .collect(),
            )),
            Command::SearchPlaylists(query) => Ok(CatalogReply::Cards(
                self.provider
                    .search_playlists(&query, 0)?
                    .items
                    .iter()
                    .map(playlist_card)
                    .collect(),
            )),
            Command::CreatePlaylist(title) => {
                let playlist = self.provider.create_playlist(&title)?;
                (|| -> Result<CatalogReply> {
                    self.store.put_json("playlists", &playlist.id.to_string(), serde_json::to_string(&playlist)?)?;
                    let CatalogReply::Collections(collections) = self.command(Command::Collections)? else { return Err("не удалось прочитать плейлисты".into()) };
                    Ok(CatalogReply::PlaylistChanged(collections, Some(playlist), None))
                })().map_err(|e| format!("Плейлист создан в Яндекс Музыке, но локальный список не обновлён. Обновите список; не создавайте его повторно. {e}").into())
            }
            Command::DeletePlaylist(id) => {
                self.provider.delete_playlist(&id)?;
                (|| -> Result<CatalogReply> {
                    let key = id.to_string();
                    self.store.write(move |c| { c.execute("DELETE FROM playlists WHERE id=?1", [key])?; Ok(()) })?;
                    let CatalogReply::Collections(collections) = self.command(Command::Collections)? else { return Err("не удалось прочитать плейлисты".into()) };
                    Ok(CatalogReply::PlaylistChanged(collections, None, Some(id)))
                })().map_err(|e| format!("Плейлист удалён в Яндекс Музыке, но локальный список не обновлён. Обновите список. {e}").into())
            }
            Command::Album(id) => {
                let mut album = self.provider.album(&id)?;
                let tracks = std::mem::take(&mut album.tracks);
                let liked = self.liked_albums()?.iter().any(|a| a.id == id);
                let header = Header {
                    id: id.to_string(),
                    subtitle: names(&album.artists),
                    subtitle_id: album
                        .artists
                        .first()
                        .map_or_else(String::new, |a| a.id.to_string()),
                    liked,
                    likeable: true,
                    ..Header::default()
                };
                let title = album.title.clone();
                let cover = header_cover(&id, "album", album.cover.clone());
                *self.last_album.borrow_mut() = Some(album);
                self.page(tracks, title, header, Vec::new(), cover)
            }
            Command::Artist(id) => {
                let page = self.provider.artist(&id)?;
                let liked = self.liked_artists()?.iter().any(|a| a.id == id);
                let header = Header {
                    id: id.to_string(),
                    liked,
                    likeable: true,
                    ..Header::default()
                };
                let cover = page.artist.cover.clone().map(|uri| crate::covers::Job {
                    key: format!("h:{id}"),
                    stem: format!("artist_{}", id.to_string().replace(':', "_")),
                    uri: Some(uri),
                    size: 300,
                });
                let cards = page.albums.iter().map(album_card).collect();
                let title = page.artist.name.clone();
                *self.last_artist.borrow_mut() = Some(page.artist);
                self.page(page.tracks, title, header, cards, cover)
            }
            Command::Albums => Ok(CatalogReply::Cards(
                self.liked_albums()?.iter().map(album_card).collect(),
            )),
            Command::Artists => Ok(CatalogReply::Cards(
                self.liked_artists()?.iter().map(artist_card).collect(),
            )),
            Command::LikeAlbum(id) => {
                let mut albums = self.liked_albums()?;
                let liked = !albums.iter().any(|a| a.id == id);
                if let Err(error) = self.provider.set_album_liked(&id, liked) {
                    log::warn!(error = error.to_string().as_str(); "album like failed");
                    return Ok(CatalogReply::CollectionLiked(id, !liked));
                }
                if liked {
                    if let Some(album) = self.last_album.borrow().as_ref().filter(|a| a.id == id) {
                        albums.insert(0, album.clone());
                    }
                } else {
                    albums.retain(|a| a.id != id);
                }
                self.store
                    .put_setting(LIKED_ALBUMS, serde_json::to_string(&albums)?)?;
                Ok(CatalogReply::CollectionLiked(id, liked))
            }
            Command::LikeArtist(id) => {
                let mut artists = self.liked_artists()?;
                let liked = !artists.iter().any(|a| a.id == id);
                if let Err(error) = self.provider.set_artist_liked(&id, liked) {
                    log::warn!(error = error.to_string().as_str(); "artist like failed");
                    return Ok(CatalogReply::CollectionLiked(id, !liked));
                }
                if liked {
                    if let Some(artist) = self.last_artist.borrow().as_ref().filter(|a| a.id == id)
                    {
                        artists.insert(0, artist.clone());
                    }
                } else {
                    artists.retain(|a| a.id != id);
                }
                self.store
                    .put_setting(LIKED_ARTISTS, serde_json::to_string(&artists)?)?;
                Ok(CatalogReply::CollectionLiked(id, liked))
            }
            Command::Playlist(id) => {
                let p = self.provider.playlist(&id)?;
                let cached = self.store.read()?.query_row(
                    "SELECT EXISTS(SELECT 1 FROM playlists WHERE id=?1)",
                    [id.to_string()],
                    |r| r.get::<_, bool>(0),
                )?;
                let deletable = cached
                    && self.provider.account().ok().is_some_and(|account| {
                        id.local()
                            .split_once('_')
                            .is_some_and(|(owner, _)| owner == account.id)
                    });
                let cover = header_cover(&id, "playlist", p.cover.clone());
                self.page(
                    p.tracks,
                    p.title,
                    Header {
                        id: id.to_string(),
                        deletable,
                        ..Header::default()
                    },
                    Vec::new(),
                    cover,
                )
            }
            Command::Login(token) => {
                self.provider.login(token)?;
                Ok(CatalogReply::Account)
            }
            Command::Like(id) => {
                let liked = !self.store.read()?.query_row(
                    "SELECT EXISTS(SELECT 1 FROM likes WHERE id=?1)",
                    [id.to_string()],
                    |r| r.get::<_, bool>(0),
                )?;
                if let Err(error) = self.provider.set_liked(&id, liked) {
                    // The UI already flipped the heart; tell it the real state.
                    return Ok(CatalogReply::LikeFailed(id, !liked, error.to_string()));
                }
                let key = id.to_string();
                self.store.write(move |c| {
                    c.execute(
                        if liked {
                            "INSERT OR IGNORE INTO likes VALUES(?1)"
                        } else {
                            "DELETE FROM likes WHERE id=?1"
                        },
                        [key],
                    )?;
                    Ok(())
                })?;
                Ok(CatalogReply::Liked(id, liked))
            }
            _ => Err("команда не относится к каталогу".into()),
        }
    }
}

impl Clone for PlayerState {
    fn clone(&self) -> Self {
        Self {
            track: self.track.clone(),
            status: self.status,
            volume: self.volume,
            liked: self.liked,
            shuffle: self.shuffle,
            repeat: self.repeat,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn audible_anchor_accounts_for_latency_and_pause() {
        let now = Instant::now();
        let mut a = PositionAnchor {
            track: 0,
            position: Duration::from_secs(2),
            audible_at: now + Duration::from_millis(100),
            state: State::Playing,
        };
        assert_eq!(position_now(&a, now), Duration::from_millis(1900));
        assert_eq!(
            position_now(&a, now + Duration::from_millis(300)),
            Duration::from_millis(2200)
        );
        a.state = State::Paused;
        assert_eq!(position_now(&a, now + Duration::from_secs(4)), a.position);
    }
}

#[cfg(test)]
mod delta_tests {
    use super::*;
    fn row(id: u32, liked: bool) -> LibraryItem {
        LibraryItem {
            track: Arc::new(Track {
                id: format!("yandex:{id}").parse().unwrap(),
                title: id.to_string(),
                artists: vec![],
                album: None,
                album_title: None,
                duration_ms: 1000,
                cover: None,
                availability: Availability::Available,
            }),
            liked,
        }
    }
    #[test]
    fn large_collections_only_reset_for_structural_changes() {
        let rows: Vec<_> = (0..10_000).map(|id| row(id, true)).collect();
        let identical = rows.clone();
        assert!(!requires_reset(&rows, &identical));
        let mut same = rows.clone();
        assert!(reconcile(&mut same, identical).is_empty());
        let mut reversed = rows.clone();
        reversed.reverse();
        assert!(requires_reset(&rows, &reversed));
        let replacement: Vec<_> = (10_000..20_000).map(|id| row(id, true)).collect();
        assert!(requires_reset(&rows, &replacement));
    }
    #[test]
    fn queue_assigns_distinct_sources_to_repeated_shared_tracks() {
        let repeated = row(1, false);
        let rows = vec![repeated.clone(), row(2, false), repeated];
        let (queue, selected) = initial_queue(&rows, 2).unwrap();
        assert_eq!(selected, 2);
        assert!(!Arc::ptr_eq(&queue[0], &queue[2]));
        assert!(Arc::ptr_eq(&queue[0], &rows[0].track));
        assert!(Arc::ptr_eq(&queue[1], &rows[1].track));
        assert_eq!(playback_order(&queue, &queue).unwrap(), [0, 1, 2]);
    }
    #[test]
    fn upcoming_wraps_only_for_repeat_all() {
        let queue = vec![
            row(1, false).track,
            row(2, false).track,
            row(3, false).track,
        ];
        let ids = |repeat| {
            upcoming(&queue, 1, repeat)
                .iter()
                .map(|t| t.id.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(Repeat::Off), ["yandex:3"]);
        assert_eq!(ids(Repeat::One), ["yandex:3"]);
        assert_eq!(ids(Repeat::All), ["yandex:3", "yandex:1"]);
    }
    #[test]
    fn repeated_source_resets_position_and_rotated_sources_map_to_queue() {
        let root =
            std::env::temp_dir().join(format!("eagle-session-mapping-{}", std::process::id()));
        let (events_tx, events_rx) = mpsc::channel();
        let (tx, _rx) = mpsc::channel();
        let mut worker = Worker::new(
            root.clone(),
            Arc::new(move |event| {
                let _ = events_tx.send(event);
            }),
            tx,
        )
        .unwrap();
        worker.catalog.take();
        worker.catalog_worker.take().unwrap().join().unwrap();
        let a = row(1, false).track;
        let b = row(2, false).track;
        worker.queue = vec![a.clone(), b.clone()];
        worker.engine_queue = vec![b, a.clone()];
        worker.current = 0;
        worker.last_position = Some(PositionAnchor {
            track: 1,
            position: Duration::from_secs(20),
            audible_at: Instant::now(),
            state: State::Playing,
        });
        worker.engine_event(eagle_engine::Event::TrackChanged(1));
        assert_eq!(worker.current, 0);
        assert!(worker.last_position.is_none());
        assert!(events_rx.try_iter().any(|event| matches!(event, Event::Player(state) if state.track.as_ref().is_some_and(|track| Arc::ptr_eq(track, &a)))));
        let anchor = PositionAnchor {
            track: 1,
            position: Duration::ZERO,
            audible_at: Instant::now(),
            state: State::Playing,
        };
        worker.engine_event(eagle_engine::Event::Position(anchor));
        assert_eq!(
            worker.last_position.as_ref().unwrap().position,
            Duration::ZERO
        );
        worker.engine_event(eagle_engine::Event::TrackChanged(0));
        assert_eq!(worker.current, 1);
        worker.engine_event(eagle_engine::Event::Position(PositionAnchor {
            track: 0,
            position: Duration::from_secs(3),
            audible_at: Instant::now(),
            state: State::Paused,
        }));
        worker.engine = Some(Engine::new(Config::default()).unwrap());
        Arc::make_mut(&mut worker.player).status = State::Paused;
        let generation = worker.engine_generation;
        let position = worker.last_position.as_ref().unwrap().position;
        for command in [
            Command::ToggleShuffle,
            Command::ToggleShuffle,
            Command::CycleRepeat,
            Command::CycleRepeat,
            Command::CycleRepeat,
        ] {
            worker.command(command).unwrap();
            assert_eq!(worker.engine_generation, generation);
            assert_eq!(worker.player.status, State::Paused);
            assert_eq!(worker.last_position.as_ref().unwrap().position, position);
            assert_eq!(worker.current, 1);
        }
        assert_eq!(worker.player.repeat, Repeat::Off);
        // No catalog is attached: blank searches must only invalidate local results.
        for command in [
            Command::Search(" ".into()),
            Command::SearchArtists("".into()),
            Command::SearchAlbums("\t".into()),
            Command::SearchPlaylists("".into()),
        ] {
            worker.rows = vec![row(1, false)];
            let generation = worker.view_generation;
            worker.command(command).unwrap();
            assert_eq!(worker.view_generation, generation + 1);
            assert!(worker.rows.is_empty());
            assert!(worker.cards.is_empty());
        }
        drop(worker);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn delayed_playlist_completion_keeps_new_view() {
        let root =
            std::env::temp_dir().join(format!("eagle-session-playlist-{}", std::process::id()));
        let (events_tx, events_rx) = mpsc::channel();
        let (tx, _rx) = mpsc::channel();
        let mut worker = Worker::new(
            root.clone(),
            Arc::new(move |event| {
                let _ = events_tx.send(event);
            }),
            tx,
        )
        .unwrap();
        worker.view = View::Search;
        worker.view_generation = 20;
        let id: TrackId = "yandex:1_13".parse().unwrap();
        let playlist = Playlist {
            id: id.clone(),
            title: "new".into(),
            tracks: vec![],
            cover: None,
        };
        worker
            .playlist_changed(19, vec![], Some(playlist), None)
            .unwrap();
        assert_eq!(worker.view, View::Search);
        assert!(
            events_rx
                .try_iter()
                .any(|event| matches!(event, Event::PlaylistCreated(created) if created == id))
        );
        worker.view = View::Playlist;
        worker.playlist_id = Some("yandex:1_14".parse().unwrap());
        worker.playlist_changed(20, vec![], None, Some(id)).unwrap();
        assert_eq!(worker.view, View::Playlist);
        worker.catalog.take();
        worker.catalog_worker.take().unwrap().join().unwrap();
        drop(worker);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn playback_indices_survive_shuffle_and_duplicate_track_ids() {
        let first = row(1, false).track;
        let second = row(2, false).track;
        let duplicate = row(1, false).track;
        let sources = vec![second.clone(), duplicate.clone(), first.clone()];
        let queue = vec![first.clone(), second, duplicate.clone()];
        assert_eq!(playback_order(&queue, &sources).unwrap(), [2, 0, 1]);
        assert_eq!(
            playback_order(&[duplicate, first], &sources).unwrap(),
            [1, 2]
        );
        assert!(playback_order(&[row(3, false).track], &sources).is_err());
    }
    #[test]
    fn changes_apply_in_order_and_identical_reload_has_no_events() {
        let mut rows = vec![row(1, false), row(2, false), row(3, false)];
        assert!(reconcile(&mut rows, vec![row(1, false), row(2, false), row(3, false)]).is_empty());
        let mut ui = rows.clone();
        for delta in reconcile(&mut rows, vec![row(2, true), row(4, false)]) {
            match delta {
                LibraryDelta::Insert { index, item } => ui.insert(index, item),
                LibraryDelta::Remove { index } => {
                    ui.remove(index);
                }
                LibraryDelta::Update { index, item } => ui[index] = item,
            }
        }
        assert_eq!(
            ui.iter()
                .map(|r| (&r.track.id, r.liked))
                .collect::<Vec<_>>(),
            rows.iter()
                .map(|r| (&r.track.id, r.liked))
                .collect::<Vec<_>>()
        );
        assert_eq!(rows.len(), 2);
        assert!(rows[0].liked);
        assert_eq!(
            reconcile(&mut rows, vec![row(2, false), row(4, false)]).len(),
            1
        );
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    #[test]
    fn cached_library_is_available_offline_and_shutdown_joins_workers() {
        let root = std::env::temp_dir().join(format!("eagle-session-{}", std::process::id()));
        {
            let store = Store::open(&root).unwrap();
            store
                .put_track(&Track {
                    id: "yandex:1".parse().unwrap(),
                    title: "Локальная медиатека".into(),
                    artists: vec![],
                    album: None,
                    album_title: None,
                    duration_ms: 1000,
                    cover: None,
                    availability: Availability::Available,
                })
                .unwrap();
            store.replace_likes(vec!["yandex:1".into()]).unwrap();
        }
        let (tx, rx) = mpsc::channel();
        let session = Session::start(root.clone(), move |event| {
            let _ = tx.send(event);
        })
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let event = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap();
            if let Event::Library(delta) = event {
                assert_eq!(delta.len(), 1);
                break;
            }
        }
        session
            .sender()
            .send(Command::Open(View::Search))
            .unwrap_or_else(|_| panic!("worker stopped"));
        loop {
            if matches!(
                rx.recv_timeout(Duration::from_secs(2)).unwrap(),
                Event::View(View::Search)
            ) {
                break;
            }
        }
        drop(session);
        while rx.recv_timeout(Duration::from_secs(2)).is_ok() {}
        let store = Store::open(&root).unwrap();
        assert!(store.track(&"yandex:1".parse().unwrap()).unwrap().is_some());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod stale_engine_tests {
    use super::*;
    #[test]
    fn events_queued_before_stop_cannot_restore_playing_state() {
        let root = std::env::temp_dir().join(format!("eagle-stale-events-{}", std::process::id()));
        let (tx, rx) = mpsc::channel();
        let states = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = states.clone();
        let mut worker = Worker::new(
            root.clone(),
            Arc::new(move |event| {
                if let Event::Player(state) = event {
                    observed.lock().unwrap().push(state.status);
                }
            }),
            tx.clone(),
        )
        .unwrap();
        tx.send(Message::Command(Command::Stop))
            .unwrap_or_else(|_| panic!("worker stopped"));
        tx.send(Message::Engine(
            0,
            eagle_engine::Event::StateChanged(State::Playing),
        ))
        .unwrap_or_else(|_| panic!("worker stopped"));
        tx.send(Message::Command(Command::Shutdown))
            .unwrap_or_else(|_| panic!("worker stopped"));
        worker.run(rx).unwrap();
        assert!(
            states
                .lock()
                .unwrap()
                .iter()
                .all(|state| *state == State::Idle)
        );
        drop(worker);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod sync_tests {
    use super::*;
    use eagle_core::*;
    struct Fixture {
        fail: bool,
        empty: bool,
        gate: Option<Arc<(std::sync::Barrier, std::sync::Barrier)>>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Provider for Fixture {
        fn name(&self) -> &str {
            "yandex"
        }
        fn account(&self) -> std::result::Result<Account, ProviderError> {
            Err(ProviderError::Unavailable)
        }
        fn login(&self, _: Token) -> std::result::Result<Account, ProviderError> {
            self.account()
        }
        fn track(&self, _: &TrackId) -> std::result::Result<Track, ProviderError> {
            Err(ProviderError::Unavailable)
        }
        fn album(&self, id: &TrackId) -> std::result::Result<Album, ProviderError> {
            Ok(Album {
                id: id.clone(),
                title: "opened album".into(),
                artists: vec![],
                tracks: vec![Track {
                    id: "yandex:22".parse()?,
                    title: "track".into(),
                    artists: vec![],
                    album: Some(id.clone()),
                    album_title: None,
                    duration_ms: 1,
                    cover: None,
                    availability: Availability::Available,
                }],
                cover: Some("image/%%".into()),
                year: None,
            })
        }
        fn playlist(&self, _: &TrackId) -> std::result::Result<Playlist, ProviderError> {
            Err(ProviderError::Unavailable)
        }
        fn search(&self, _: &str, _: u32) -> std::result::Result<Page<Track>, ProviderError> {
            Err(ProviderError::Unavailable)
        }
        fn search_artists(
            &self,
            query: &str,
            _: u32,
        ) -> std::result::Result<Page<Artist>, ProviderError> {
            Ok(Page {
                items: if query == "empty" {
                    vec![]
                } else {
                    vec![Artist {
                        id: "yandex:11".parse()?,
                        name: "artist".into(),
                        cover: None,
                    }]
                },
                next: None,
            })
        }
        fn search_albums(
            &self,
            _: &str,
            _: u32,
        ) -> std::result::Result<Page<Album>, ProviderError> {
            Ok(Page {
                items: vec![Album {
                    id: "yandex:12".parse()?,
                    title: "album".into(),
                    artists: vec![],
                    tracks: vec![],
                    cover: None,
                    year: None,
                }],
                next: None,
            })
        }
        fn search_playlists(
            &self,
            _: &str,
            _: u32,
        ) -> std::result::Result<Page<Playlist>, ProviderError> {
            Ok(Page {
                items: vec![Playlist {
                    id: "yandex:1_13".parse()?,
                    title: "playlist".into(),
                    tracks: vec![],
                    cover: None,
                }],
                next: None,
            })
        }
        fn create_playlist(&self, title: &str) -> std::result::Result<Playlist, ProviderError> {
            if self.fail {
                return Err(ProviderError::Network);
            }
            Ok(Playlist {
                id: "yandex:1_13".parse()?,
                title: title.into(),
                tracks: vec![],
                cover: None,
            })
        }
        fn delete_playlist(&self, _: &TrackId) -> std::result::Result<(), ProviderError> {
            if self.fail {
                Err(ProviderError::Auth)
            } else {
                Ok(())
            }
        }
        fn playlists(&self, _: Option<&str>) -> std::result::Result<Page<Playlist>, ProviderError> {
            Ok(Page {
                items: vec![],
                next: None,
            })
        }
        fn likes(&self, cursor: Option<&str>) -> std::result::Result<Page<Track>, ProviderError> {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if cursor.is_none()
                && call == 0
                && let Some(gate) = &self.gate
            {
                gate.0.wait();
                gate.1.wait();
            }
            if self.empty {
                return Ok(Page {
                    items: vec![],
                    next: None,
                });
            }
            if cursor.is_some() && self.fail {
                return Err(ProviderError::Network);
            }
            let n = if cursor.is_some() { 2 } else { 1 };
            let track = Track {
                id: format!("yandex:{n}").parse()?,
                title: n.to_string(),
                artists: vec![],
                album: None,
                album_title: None,
                duration_ms: 1,
                cover: None,
                availability: Availability::Available,
            };
            Ok(Page {
                items: vec![track],
                next: (cursor.is_none()).then(|| "next".to_string()),
            })
        }
        fn set_liked(&self, _: &TrackId, _: bool) -> std::result::Result<(), ProviderError> {
            Ok(())
        }
        fn set_disliked(&self, _: &TrackId, _: bool) -> std::result::Result<(), ProviderError> {
            Ok(())
        }
        fn resolve_stream(
            &self,
            _: &TrackId,
            _: Quality,
        ) -> std::result::Result<Stream, ProviderError> {
            Err(ProviderError::Unavailable)
        }
    }
    fn catalog(fail: bool) -> Catalog {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "eagle-sync-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        Catalog {
            covers: RefCell::new(VecDeque::new()),
            last_album: RefCell::new(None),
            last_artist: RefCell::new(None),
            store: Arc::new(Store::open(&root).unwrap()),
            net: Arc::new(eagle_net::Client::new().unwrap()),
            provider: Arc::new(Fixture {
                fail,
                empty: false,
                gate: None,
                calls: Arc::default(),
            }),
            root,
        }
    }
    #[test]
    fn search_categories_route_cards_and_empty_results() {
        let c = catalog(false);
        for (command, prefix) in [
            (Command::SearchArtists("x".into()), "r:"),
            (Command::SearchAlbums("x".into()), "a:"),
            (Command::SearchPlaylists("x".into()), "p:"),
        ] {
            let CatalogReply::Cards(cards) = c.command(command).unwrap() else {
                panic!("expected cards")
            };
            assert_eq!(cards.len(), 1);
            assert!(cards[0].key.starts_with(prefix));
        }
        assert!(
            matches!(c.command(Command::SearchArtists("empty".into())).unwrap(), CatalogReply::Cards(cards) if cards.is_empty())
        );
    }
    #[test]
    fn album_opens_tracks_header_and_own_artwork() {
        let c = catalog(false);
        let id: TrackId = "yandex:12".parse().unwrap();
        let CatalogReply::Page {
            items,
            title,
            header,
            cover,
            ..
        } = c.command(Command::Album(id.clone())).unwrap()
        else {
            panic!("expected album page")
        };
        assert_eq!(title, "opened album");
        assert_eq!(items[0].track.id.to_string(), "yandex:22");
        assert_eq!(header.id, id.to_string());
        let cover = cover.unwrap();
        assert_eq!(cover.key, format!("h:{id}"));
        assert_eq!(cover.size, 600);
    }
    #[test]
    fn playlist_mutations_update_cache_only_after_provider_success() {
        let mut c = catalog(false);
        let id: TrackId = "yandex:1_13".parse().unwrap();
        let CatalogReply::PlaylistChanged(items, Some(created), None) =
            c.command(Command::CreatePlaylist("new".into())).unwrap()
        else {
            panic!("expected creation")
        };
        assert!(created.tracks.is_empty());
        assert_eq!(items[0].id, id);
        c.provider = Arc::new(Fixture {
            fail: true,
            empty: false,
            gate: None,
            calls: Arc::default(),
        });
        assert!(c.command(Command::DeletePlaylist(id.clone())).is_err());
        assert!(
            c.command(Command::CreatePlaylist("failure".into()))
                .is_err()
        );
        assert!(
            matches!(c.command(Command::Collections).unwrap(), CatalogReply::Collections(items) if items.len() == 1)
        );
        c.provider = Arc::new(Fixture {
            fail: false,
            empty: false,
            gate: None,
            calls: Arc::default(),
        });
        assert!(
            matches!(c.command(Command::DeletePlaylist(id)).unwrap(), CatalogReply::PlaylistChanged(items, None, Some(_)) if items.is_empty())
        );
    }
    #[test]
    fn remote_success_and_cache_failure_report_partial_success() {
        let c = catalog(false);
        c.store
            .write(|db| {
                db.execute("DROP TABLE playlists", [])?;
                Ok(())
            })
            .unwrap();
        let create = match c.command(Command::CreatePlaylist("new".into())) {
            Err(error) => error.to_string(),
            Ok(_) => panic!("expected cache failure"),
        };
        assert!(create.contains("создан в Яндекс Музыке"));
        assert!(create.contains("не создавайте его повторно"));
        let delete = match c.command(Command::DeletePlaylist("yandex:1_13".parse().unwrap())) {
            Err(error) => error.to_string(),
            Ok(_) => panic!("expected cache failure"),
        };
        assert!(delete.contains("удалён в Яндекс Музыке"));
    }
    #[test]
    fn catalog_drains_commands_and_cancels_old_account_sync() {
        let mut c = catalog(false);
        let gate = Arc::new((std::sync::Barrier::new(2), std::sync::Barrier::new(2)));
        c.provider = Arc::new(Fixture {
            fail: false,
            empty: false,
            gate: Some(gate.clone()),
            calls: Arc::default(),
        });
        c.store.replace_likes(vec!["yandex:99".into()]).unwrap();
        let store = c.store.clone();
        let (tx, rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || c.run(rx, reply_tx));
        tx.send(CatalogRequest::Command(0, Command::SyncLibrary))
            .unwrap();
        gate.0.wait();
        tx.send(CatalogRequest::Command(10, Command::Collections))
            .unwrap();
        tx.send(CatalogRequest::Command(
            11,
            Command::Login(Token::new("new".into())),
        ))
        .unwrap();
        gate.1.wait();
        let released = Instant::now();
        let mut seen = Vec::new();
        while seen.len() < 2 {
            if let Message::Catalog(generation, _) =
                reply_rx.recv_timeout(Duration::from_secs(5)).unwrap()
            {
                if generation != 0 {
                    if generation == 10 {
                        println!(
                            "catalog command latency after page release: {:?}",
                            released.elapsed()
                        );
                    }
                    seen.push(generation);
                }
            }
        }
        assert_eq!(seen, [10, 11]);
        drop(tx);
        worker.join().unwrap();
        let count: u32 = store
            .read()
            .unwrap()
            .query_row("SELECT count(*) FROM likes WHERE id='yandex:99'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
    }
    #[test]
    fn like_during_page_restarts_sync_before_publication() {
        let mut c = catalog(false);
        let gate = Arc::new((std::sync::Barrier::new(2), std::sync::Barrier::new(2)));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        c.provider = Arc::new(Fixture {
            fail: false,
            empty: false,
            gate: Some(gate.clone()),
            calls: calls.clone(),
        });
        c.store.replace_likes(vec!["yandex:99".into()]).unwrap();
        let (tx, rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || c.run(rx, reply_tx));
        tx.send(CatalogRequest::Command(0, Command::SyncLibrary))
            .unwrap();
        gate.0.wait();
        tx.send(CatalogRequest::Command(
            10,
            Command::Like("yandex:3".parse().unwrap()),
        ))
        .unwrap();
        gate.1.wait();
        let mut liked = false;
        loop {
            match reply_rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                Message::Catalog(10, Ok(CatalogReply::Liked(_, _))) => liked = true,
                Message::Catalog(_, Ok(CatalogReply::Synced)) => {
                    assert!(liked);
                    break;
                }
                _ => {}
            }
        }
        drop(tx);
        worker.join().unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    }
    #[test]
    fn repeated_sync_requests_are_coalesced() {
        let mut c = catalog(false);
        let gate = Arc::new((std::sync::Barrier::new(2), std::sync::Barrier::new(2)));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        c.provider = Arc::new(Fixture {
            fail: false,
            empty: false,
            gate: Some(gate.clone()),
            calls: calls.clone(),
        });
        let (tx, rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || c.run(rx, reply_tx));
        tx.send(CatalogRequest::Command(0, Command::SyncLibrary))
            .unwrap();
        gate.0.wait();
        for _ in 0..5 {
            tx.send(CatalogRequest::Command(0, Command::SyncLibrary))
                .unwrap();
            tx.send(CatalogRequest::Command(0, Command::SyncCollections))
                .unwrap();
        }
        gate.1.wait();
        loop {
            if let Message::Catalog(_, Ok(CatalogReply::Synced)) =
                reply_rx.recv_timeout(Duration::from_secs(5)).unwrap()
            {
                break;
            }
        }
        tx.send(CatalogRequest::Command(10, Command::Collections))
            .unwrap();
        loop {
            if let Message::Catalog(10, _) = reply_rx.recv_timeout(Duration::from_secs(5)).unwrap()
            {
                break;
            }
        }
        drop(tx);
        worker.join().unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
    #[test]
    fn empty_page_clears_likes_only_on_publication() {
        let mut c = catalog(false);
        c.provider = Arc::new(Fixture {
            fail: false,
            empty: true,
            gate: None,
            calls: Arc::default(),
        });
        c.store.replace_likes(vec!["yandex:99".into()]).unwrap();
        let mut state = SyncState::default();
        c.sync_step(&mut state).unwrap();
        c.sync_step(&mut state).unwrap();
        let count = || {
            c.store
                .read()
                .unwrap()
                .query_row("SELECT count(*) FROM likes", [], |r| r.get::<_, u32>(0))
                .unwrap()
        };
        assert_eq!(count(), 1);
        c.sync_step(&mut state).unwrap();
        assert_eq!(count(), 0);
    }
    #[test]
    fn pages_publish_only_after_success_and_commands_interleave() {
        for fail in [false, true] {
            let c = catalog(fail);
            c.store.replace_likes(vec!["yandex:99".into()]).unwrap();
            let mut state = SyncState::default();
            c.sync_step(&mut state).unwrap();
            c.sync_step(&mut state).unwrap();
            assert!(matches!(
                c.command(Command::Collections).unwrap(),
                CatalogReply::Collections(_)
            ));
            let count = || {
                c.store
                    .read()
                    .unwrap()
                    .query_row("SELECT count(*) FROM likes WHERE id='yandex:99'", [], |r| {
                        r.get::<_, u32>(0)
                    })
                    .unwrap()
            };
            assert_eq!(count(), 1);
            if fail {
                assert!(c.sync_step(&mut state).is_err());
                assert_eq!(count(), 1);
            } else {
                c.sync_step(&mut state).unwrap();
                assert_eq!(count(), 1);
                c.sync_step(&mut state).unwrap();
                assert_eq!(count(), 0);
                let conn = c.store.read().unwrap();
                let mut stmt = conn
                    .prepare("SELECT id FROM likes ORDER BY rowid DESC")
                    .unwrap();
                let ids: Vec<String> = stmt
                    .query_map([], |r| r.get(0))
                    .unwrap()
                    .map(|r| r.unwrap())
                    .collect();
                assert_eq!(ids, ["yandex:1", "yandex:2"]);
            }
        }
    }
}
