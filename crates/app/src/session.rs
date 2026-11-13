// SPDX-License-Identifier: GPL-3.0-or-later
//! Desktop-facing commands and incremental events. No UI types cross this boundary.
use crate::providers::{
    ProviderStatus, Providers, account_key, liked_albums_key, liked_artists_key,
};
use eagle_core::{
    Album, Artist, AuthMethod, Capabilities, Playlist, PlaylistEdit, PlaylistEditError,
    PlaylistSnapshot, PlaylistTrack, Provider, ProviderError, RadioBatch, RadioError,
    RadioQueueItem, RadioSession, Track, TrackId,
};
pub use eagle_core::{Availability, Quality, Token};
use eagle_engine::{Config, Engine, PositionAnchor, State};
pub use eagle_net::policy::NetworkStatus;
pub use eagle_store::DownloadState;
use eagle_store::{Cache, Store};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
/// Above this many inserts/removals a list change is sent as one reset.
const RESET_THRESHOLD: usize = 32;
/// Wait before retrying likes that a provider refused for rate limiting.
fn likes_backfill_key(provider: &str) -> String {
    format!("likes_backfill:{provider}")
}
/// Likes sent per flush. The rest stay queued so that user requests run between batches.
const LIKE_BATCH: usize = 20;
/// Seconds since the epoch, read by likes retries. Tests replace it.
type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;
fn wall_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64
    })
}

/// The uid of the account owning this installation's rights, or empty when none is stored.
fn stored_account(store: &Store, providers: &Providers) -> Result<String> {
    Ok(match providers.account_provider() {
        Some(provider) => store
            .setting(&account_key(provider.info().id))?
            .unwrap_or_default(),
        None => String::new(),
    })
}

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
        sources: Vec::new(),
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
        sources: Vec::new(),
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
        sources: Vec::new(),
    }
}
/// The first page of cards of one kind from `provider`.
fn cards_of(
    provider: &dyn Provider,
    kind: CardKind,
    query: &str,
) -> std::result::Result<Vec<Card>, ProviderError> {
    Ok(match kind {
        CardKind::Artists => provider
            .search_artists(query, 0)?
            .items
            .iter()
            .map(artist_card)
            .collect(),
        CardKind::Albums => provider
            .search_albums(query, 0)?
            .items
            .iter()
            .map(album_card)
            .collect(),
        CardKind::Playlists => provider
            .search_playlists(query, 0)?
            .items
            .iter()
            .map(playlist_card)
            .collect(),
    })
}
fn cards_reply(rows: &[(Card, Option<u64>)]) -> CatalogReply {
    CatalogReply::Cards(rows.iter().map(|(card, _)| card.clone()).collect())
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
    Radio,
    Saved,
    History,
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
    RadioStart,
    RadioMore,
    UiAttached(bool),
    ClearPlaylistMutationNotice(TrackId),
    EditPlaylist {
        id: TrackId,
        revision: u64,
        edit: PlaylistEdit,
    },
    AddTrackToPlaylist {
        id: TrackId,
        revision: u64,
        entry: Box<PlaylistTrack>,
    },
    /// One coherent state reply, ordered before subsequent worker events.
    Snapshot,
    Downloads,
    SaveAll(Quality),
    CacheLimit(u64),
    AllowMetered(bool),
    RelocateStorage {
        destination: PathBuf,
        bootstrap: PathBuf,
        lease: Option<Arc<eagle_store::relocation::StorageLease>>,
    },
    RemoveSaved(i64),
    Save {
        id: TrackId,
        quality: Quality,
    },
    PauseDownload(i64),
    CancelDownload(i64),
    RetryDownload(i64),
    /// Move an existing occurrence, identified independently of the track ID.
    MoveQueue {
        id: u64,
        to: usize,
    },
    ShiftQueue {
        id: u64,
        delta: i32,
    },
    PlayNext(u64),
    /// Add a fresh occurrence from local metadata immediately after the current one.
    EnqueueNext(TrackId),
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
    /// Signs in to one provider with a token the user typed.
    Login {
        provider: String,
        token: Token,
    },
    /// Forgets the provider's token. Offline saves of its account go too unless `keep_saved`;
    /// likes and playlists stay.
    Logout {
        provider: String,
        keep_saved: bool,
    },
    /// The user's answer to `Event::LikesBackfillOffer`: send the likes the provider lacks, now
    /// and whenever a new link appears (`accept`), or never.
    LikesBackfill {
        provider: String,
        accept: bool,
    },
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
    /// Play the exact visible row, including duplicate track IDs.
    PlayRow(usize),
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
    pub blocked_reason: Option<String>,
    /// Other providers' copies of the same track (search results merge by group); empty otherwise.
    pub sources: Vec<TrackId>,
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
    pub bitrate_kbps: Option<u32>,
    /// Provider id of the copy actually playing; may differ from `track.id`'s provider.
    pub source_provider: Option<String>,
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
            bitrate_kbps: None,
            source_provider: None,
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
    /// Ids of the same artist on other providers, folded into this card. Not shown in the UI.
    pub sources: Vec<TrackId>,
}
/// The kinds of cards that search can return.
#[derive(Clone, Copy, Debug)]
enum CardKind {
    Artists,
    Albums,
    Playlists,
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
    pub editable: bool,
    pub playlist_revision: Option<u64>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Collection {
    pub id: TrackId,
    pub title: String,
}
#[derive(Clone)]
pub struct SessionSnapshot {
    pub view: View,
    pub rows: Vec<LibraryItem>,
    pub cards: Vec<Card>,
    pub collections: Vec<Collection>,
    pub title: String,
    pub length: Duration,
    pub header: Header,
    pub busy: bool,
    pub playlist_mutation_busy: bool,
    pub playlist_mutation_notice: Option<String>,
    pub radio_ready: bool,
    pub radio_busy: bool,
    pub radio_notice: Option<String>,
    pub signed_in: bool,
    pub player: Arc<PlayerState>,
    pub queue: Vec<(u64, Arc<Track>)>,
    pub current: Option<u64>,
    pub position: Option<PositionAnchor>,
}
pub enum Event {
    /// Every provider with its sign-in state; sent whenever any of them changes.
    Providers(Vec<ProviderStatus>),
    /// `count` local likes could be sent to the connected `provider`; nothing is sent until the
    /// user answers with `Command::LikesBackfill`.
    LikesBackfillOffer {
        provider: String,
        count: usize,
    },
    RadioState {
        ready: bool,
        busy: bool,
        notice: Option<String>,
    },
    Snapshot(Box<SessionSnapshot>),
    Downloads(Vec<eagle_store::DownloadJob>),
    NetworkPolicy(eagle_net::policy::NetworkPolicy),
    StorageMoving(bool),
    Storage {
        staging_bytes: u64,
        other_bytes: u64,
        total_bytes: u64,
        cache_bytes: u64,
        saved_bytes: u64,
        limit: u64,
        folder: PathBuf,
    },
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
    QueueOrder {
        entries: Vec<(u64, Arc<Track>)>,
        current: Option<u64>,
    },
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
    if rows.get(selected).is_none_or(|row| {
        row.blocked_reason.is_some() || row.track.availability != Availability::Available
    }) {
        return Err("трек недоступен".into());
    }
    let current = rows[..selected]
        .iter()
        .filter(|row| {
            row.blocked_reason.is_none() && row.track.availability == Availability::Available
        })
        .count();
    let mut seen = HashSet::new();
    let queue = rows
        .iter()
        .filter(|row| {
            row.blocked_reason.is_none() && row.track.availability == Availability::Available
        })
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

fn select_play_all(
    rows: &[LibraryItem],
    shuffle: bool,
    mut key: impl FnMut(usize) -> u64,
) -> Option<usize> {
    let available = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| {
            row.blocked_reason.is_none() && row.track.availability == Availability::Available
        })
        .map(|(index, _)| index);
    if shuffle {
        available.min_by_key(|index| key(*index))
    } else {
        available.into_iter().next()
    }
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
            if rows[index].liked != item.liked
                || rows[index].track != item.track
                || rows[index].blocked_reason != item.blocked_reason
            {
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
    Catalog(u64, Box<std::result::Result<CatalogReply, String>>),
    Engine(u64, eagle_engine::Event),
    Bitrate(u64, u64, Option<u32>),
    /// The copy of the track that was really chosen for playback.
    Source(u64, u64, TrackId),
    /// The network became reachable: deferred likes are sent now.
    NetworkOnline,
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
        Self::start_inner(root, None, Arc::new(crate::tokens::Keyring), emit)
    }
    /// Starts an isolated session without consulting the system credential store.
    pub fn start_with_providers(
        root: PathBuf,
        providers: Vec<Arc<dyn Provider>>,
        emit: impl Fn(Event) + Send + Sync + 'static,
    ) -> std::io::Result<Self> {
        Self::start_inner(
            root,
            Some(providers),
            Arc::new(crate::tokens::MemoryTokens::default()),
            emit,
        )
    }
    fn start_inner(
        root: PathBuf,
        providers: Option<Vec<Arc<dyn Provider>>>,
        tokens: Arc<dyn crate::tokens::TokenStore>,
        emit: impl Fn(Event) + Send + Sync + 'static,
    ) -> std::io::Result<Self> {
        let (commands, rx) = mpsc::channel();
        let tx = commands.clone();
        let emit = Arc::new(emit);
        let worker = thread::Builder::new()
            .name("eg-app".into())
            .spawn(move || {
                if let Err(e) = Worker::new_with_providers(
                    root,
                    emit.clone(),
                    tx,
                    providers,
                    tokens,
                    wall_clock(),
                )
                .and_then(|w| w.run(rx))
                {
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
    providers: Arc<Providers>,
    catalog: Option<mpsc::Sender<CatalogRequest>>,
    catalog_worker: Option<thread::JoinHandle<()>>,
    engine: Option<Engine>,
    engine_events: Option<thread::JoinHandle<()>>,
    engine_generation: u64,
    audio_bitrates: HashMap<u64, Option<u32>>,
    /// Provider id of the copy that plays, per queue occurrence.
    audio_sources: HashMap<u64, String>,
    tx: mpsc::Sender<Message>,
    rows: Vec<LibraryItem>,
    queue: Vec<Arc<Track>>,
    queue_likes: HashSet<TrackId>,
    queue_original: Vec<Arc<Track>>,
    queue_ids: HashMap<usize, u64>,
    queue_radio: HashMap<u64, eagle_store::RadioContext>,
    next_queue_id: u64,
    current: usize,
    /// Owns history, playback position and the queue; survives account changes.
    profile: String,
    /// Uid of the account that owns rights, downloads and play reports; empty when signed out.
    account: String,
    last_checkpoint: Instant,
    history_recorded: bool,
    history_occurrence: Option<u64>,
    history_marker: Option<(u32, Option<Duration>)>,
    restored_position: Duration,
    collections: Vec<Collection>,
    title: String,
    length: Duration,
    header: Header,
    header_job: Option<crate::covers::Job>,
    ui_attached: bool,
    cover_generation: u64,
    busy: bool,
    engine_queue: Vec<Arc<Track>>,
    view_generation: u64,
    view: View,
    playlist_id: Option<TrackId>,
    playlist_snapshot: Option<PlaylistSnapshot>,
    playlist_edit_busy: bool,
    playlist_edit_target: Option<TrackId>,
    playlist_edit_notice: Option<(TrackId, String)>,
    radio: Option<(String, RadioSession, Vec<RadioBatch>)>,
    radio_busy: bool,
    radio_can_more: bool,
    radio_notice: Option<String>,
    cards: Vec<Card>,
    covers: crate::covers::Covers,
    downloads: crate::downloads::Downloads,
    last_position: Option<PositionAnchor>,
    player: Arc<PlayerState>,
    emit: Arc<dyn Fn(Event) + Send + Sync>,
    clock: Clock,
    /// When the earliest deferred like is due, if any is pending in the future.
    like_retry_at: Option<i64>,
}
impl Worker {
    #[cfg(test)]
    fn new(
        root: PathBuf,
        emit: Arc<dyn Fn(Event) + Send + Sync>,
        tx: mpsc::Sender<Message>,
    ) -> Result<Self> {
        Self::new_with_providers(
            root,
            emit,
            tx,
            None,
            Arc::new(crate::tokens::MemoryTokens::default()),
            wall_clock(),
        )
    }
    fn new_with_providers(
        root: PathBuf,
        emit: Arc<dyn Fn(Event) + Send + Sync>,
        tx: mpsc::Sender<Message>,
        providers: Option<Vec<Arc<dyn Provider>>>,
        tokens: Arc<dyn crate::tokens::TokenStore>,
        clock: Clock,
    ) -> Result<Self> {
        Self::new_with_handles(root, emit, tx, providers, tokens, None, clock)
    }
    fn new_with_handles(
        root: PathBuf,
        emit: Arc<dyn Fn(Event) + Send + Sync>,
        tx: mpsc::Sender<Message>,
        providers: Option<Vec<Arc<dyn Provider>>>,
        tokens: Arc<dyn crate::tokens::TokenStore>,
        handles: Option<(Arc<Store>, Arc<Cache>)>,
        clock: Clock,
    ) -> Result<Self> {
        let (store, cache) = if let Some(handles) = handles {
            handles
        } else {
            let store = Arc::new(Store::open(&root)?);
            let cache = Arc::new(Cache::open(
                &root.join("audio"),
                store
                    .setting("cache_limit")?
                    .and_then(|value| value.parse::<u64>().ok())
                    .filter(|value| (256 * 1024 * 1024..=128 * 1024 * 1024 * 1024).contains(value))
                    .unwrap_or(2 * 1024 * 1024 * 1024),
                &store,
            )?);
            (store, cache)
        };
        let net = Arc::new(eagle_net::Client::new()?);
        let providers = providers.unwrap_or_else(|| {
            #[cfg(not(test))]
            {
                crate::providers::builtin(net.clone())
            }
            // Session tests must never touch the user's keychain or live account.
            #[cfg(test)]
            {
                vec![sync_tests::offline_provider()]
            }
        });
        let order: Vec<String> = match store.setting("provider_order")? {
            Some(json) => crate::providers::parse_provider_order(&json),
            None => Vec::new(),
        };
        let providers = Arc::new(Providers::new(providers, &order).with_tokens(tokens));
        let (catalog, requests) = mpsc::channel();
        let services = Catalog {
            covers: RefCell::new(VecDeque::new()),
            last_album: RefCell::new(None),
            last_artist: RefCell::new(None),
            root: root.clone(),
            net: net.clone(),
            store: store.clone(),
            providers: providers.clone(),
            likes_more: std::cell::Cell::new(false),
            clock: clock.clone(),
        };
        let replies = tx.clone();
        let catalog_worker = thread::Builder::new()
            .name("eg-catalog".into())
            .spawn(move || {
                services.run(requests, replies);
            })?;
        let covers = crate::covers::Covers::start(root.clone(), net.clone(), emit.clone())?;
        let online = tx.clone();
        let downloads = crate::downloads::Downloads::start(
            root.clone(),
            store.clone(),
            cache.clone(),
            net.clone(),
            providers.clone(),
            emit.clone(),
            Box::new(move || {
                let _ = online.send(Message::NetworkOnline);
            }),
        )?;
        let account = stored_account(&store, &providers)?;
        downloads.account(account.clone());
        let profile = store.profile_id()?;
        let mut worker = Self {
            store,
            cache,
            net,
            providers,
            catalog: Some(catalog),
            catalog_worker: Some(catalog_worker),
            engine: None,
            engine_events: None,
            engine_generation: 0,
            audio_bitrates: HashMap::new(),
            audio_sources: HashMap::new(),
            tx,
            rows: Vec::new(),
            queue: Vec::new(),
            queue_likes: HashSet::new(),
            queue_original: Vec::new(),
            queue_ids: HashMap::new(),
            queue_radio: HashMap::new(),
            next_queue_id: 1,
            current: 0,
            profile,
            account,
            last_checkpoint: Instant::now(),
            history_recorded: false,
            history_occurrence: None,
            history_marker: None,
            restored_position: Duration::ZERO,
            collections: Vec::new(),
            title: String::new(),
            length: Duration::ZERO,
            header: Header::default(),
            header_job: None,
            ui_attached: true,
            cover_generation: 0,
            busy: false,
            engine_queue: Vec::new(),
            view_generation: 0,
            view: View::Library,
            playlist_id: None,
            playlist_snapshot: None,
            playlist_edit_busy: false,
            playlist_edit_target: None,
            playlist_edit_notice: None,
            radio: None,
            radio_busy: false,
            radio_can_more: false,
            radio_notice: None,
            cards: Vec::new(),
            covers,
            downloads,
            last_position: None,
            player: Arc::default(),
            emit,
            clock,
            like_retry_at: None,
        };
        worker.restore_playback()?;
        Ok(worker)
    }
    fn restore_playback(&mut self) -> Result<()> {
        let Some(saved) = self.store.playback(&self.profile)? else {
            return Ok(());
        };
        // Entries retain the original order; order contains occurrence IDs, not track IDs.
        let entries: std::collections::HashMap<_, _> = saved
            .entries
            .iter()
            .map(|entry| (entry.id, Arc::new(entry.track.clone())))
            .collect();
        self.queue_original = saved
            .entries
            .iter()
            .map(|e| entries[&e.id].clone())
            .collect();
        self.queue_ids = entries
            .iter()
            .map(|(id, track)| (Arc::as_ptr(track) as usize, *id))
            .collect();
        // The queue belongs to the profile; radio contexts of another account are dropped
        // and their tracks stay in the queue as ordinary entries.
        self.queue_radio = saved
            .entries
            .iter()
            .filter_map(|entry| {
                entry
                    .radio
                    .clone()
                    .filter(|context| context.account == self.account)
                    .map(|context| (entry.id, context))
            })
            .collect();
        self.next_queue_id = saved
            .entries
            .iter()
            .map(|e| e.id)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or("исчерпаны идентификаторы очереди")?;
        self.queue = saved.order.iter().map(|id| entries[id].clone()).collect();
        self.current = saved
            .current
            .and_then(|id| saved.order.iter().position(|i| *i == id))
            .unwrap_or(0);
        self.restored_position = Duration::from_millis(saved.position_ms).min(
            self.queue
                .get(self.current)
                .map_or(Duration::ZERO, |t| Duration::from_millis(t.duration_ms)),
        );
        let player = Arc::make_mut(&mut self.player);
        self.queue_likes = self
            .store
            .liked_track_ids()?
            .into_iter()
            .filter_map(|id| id.parse().ok())
            .collect();
        player.track = self.queue.get(self.current).cloned();
        player.liked = player
            .track
            .as_ref()
            .is_some_and(|track| self.queue_likes.contains(&track.id));
        player.status = if player.track.is_some() {
            State::Paused
        } else {
            State::Idle
        };
        player.shuffle = saved.shuffle;
        player.repeat = match saved.repeat {
            1 => Repeat::All,
            2 => Repeat::One,
            _ => Repeat::Off,
        };
        if player.track.is_some() {
            self.last_position = Some(PositionAnchor {
                track: 0,
                position: self.restored_position,
                audible_at: Instant::now(),
                state: State::Paused,
            });
        }
        Ok(())
    }
    fn occurrence_id(&self, track: &Arc<Track>) -> u64 {
        self.queue_ids
            .get(&(Arc::as_ptr(track) as usize))
            .copied()
            .unwrap_or(0)
    }
    fn assign_occurrences(&mut self) -> Result<()> {
        for track in &self.queue_original {
            if let std::collections::hash_map::Entry::Vacant(entry) =
                self.queue_ids.entry(Arc::as_ptr(track) as usize)
            {
                let id = self.next_queue_id;
                self.next_queue_id = id
                    .checked_add(1)
                    .ok_or("исчерпаны идентификаторы очереди")?;
                entry.insert(id);
            }
        }
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<()> {
        // A full disk must not turn the periodic checkpoint into a busy retry loop.
        self.last_checkpoint = Instant::now();
        self.assign_occurrences()?;
        let position = self
            .last_position
            .as_ref()
            .map_or(self.restored_position, |anchor| {
                position_now(anchor, Instant::now())
            });
        let state = eagle_store::PlaybackState {
            entries: self
                .queue_original
                .iter()
                .map(|track| eagle_store::PlaybackEntry {
                    id: self.occurrence_id(track),
                    track: (**track).clone(),
                    radio: self.queue_radio.get(&self.occurrence_id(track)).cloned(),
                })
                .collect(),
            order: self
                .queue
                .iter()
                .map(|track| self.occurrence_id(track))
                .collect(),
            current: self
                .queue
                .get(self.current)
                .map(|track| self.occurrence_id(track)),
            position_ms: position.as_millis().min(u128::from(u64::MAX)) as u64,
            shuffle: self.player.shuffle,
            repeat: match self.player.repeat {
                Repeat::Off => 0,
                Repeat::All => 1,
                Repeat::One => 2,
            },
        };
        self.store.save_playback(&self.profile, state)?;
        self.last_checkpoint = Instant::now();
        Ok(())
    }
    fn network_policy(&self) -> Result<()> {
        let allow = self.store.setting("allow_metered")?.as_deref() == Some("true");
        let policy = eagle_net::system_network::query(allow);
        self.downloads.network_policy(policy);
        (self.emit)(Event::NetworkPolicy(policy));
        Ok(())
    }
    fn storage(&self) -> Result<()> {
        let usage = self.cache.storage_usage(&self.store)?;
        (self.emit)(Event::Storage {
            cache_bytes: usage.cache,
            saved_bytes: usage.saved,
            staging_bytes: usage.staging,
            other_bytes: usage.other,
            total_bytes: usage.total(),
            limit: self.cache.limit(),
            folder: self
                .cache
                .root()
                .parent()
                .unwrap_or(self.cache.root())
                .to_owned(),
        });
        Ok(())
    }
    fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            view: self.view,
            rows: self.rows.clone(),
            cards: self.cards.clone(),
            collections: self.collections.clone(),
            title: self.title.clone(),
            length: self.length,
            header: self.header.clone(),
            busy: self.busy,
            playlist_mutation_busy: self.playlist_edit_busy
                && self.playlist_edit_target == self.playlist_id,
            playlist_mutation_notice: self
                .playlist_edit_notice
                .as_ref()
                .filter(|(id, _)| Some(id) == self.playlist_id.as_ref())
                .map(|(_, notice)| notice.clone()),
            radio_ready: self.radio_can_more && !self.radio_busy,
            radio_busy: self.radio_busy,
            radio_notice: self.radio_notice.clone(),
            signed_in: !self.account.is_empty(),
            player: self.player.clone(),
            queue: self
                .queue
                .iter()
                .map(|track| (self.occurrence_id(track), track.clone()))
                .collect(),
            current: self
                .queue
                .get(self.current)
                .map(|track| self.occurrence_id(track)),
            position: self.last_position.clone(),
        }
    }
    fn move_queue(&mut self, id: u64, to: usize) -> Result<()> {
        if to >= self.queue.len() {
            return Err("неверная позиция очереди".into());
        }
        let from = self
            .queue
            .iter()
            .position(|track| self.occurrence_id(track) == id)
            .ok_or("элемент очереди не найден")?;
        let current = self.queue.get(self.current).cloned();
        let track = self.queue.remove(from);
        self.queue.insert(to, track);
        self.current = current
            .and_then(|track| self.queue.iter().position(|item| Arc::ptr_eq(item, &track)))
            .unwrap_or(0);
        self.configure_playback()?;
        self.emit_queue();
        Ok(())
    }
    fn run(mut self, rx: mpsc::Receiver<Message>) -> Result<()> {
        'session: loop {
            (self.emit)(Event::Player(self.player.clone()));
            self.emit_queue();
            if let Some(position) = &self.last_position {
                (self.emit)(Event::Position(position.clone()));
            }
            if self.ui_attached
                && let Some(track) = &self.player.track
                && let Some(catalog) = &self.catalog
            {
                let _ = catalog.send(CatalogRequest::Cover(
                    track.clone(),
                    CoverTarget::Player,
                    0,
                    self.cover_generation,
                ));
            }
            self.request(Command::Collections)?;
            self.request(Command::Library)?;
            (self.emit)(Event::Providers(self.providers.statuses()));
            self.background(Command::Restore)?;
            self.refresh_like_deadline();
            loop {
                if self.like_due() {
                    self.send_flush_likes(false);
                }
                let message = match self.timer_wait() {
                    None => match rx.recv() {
                        Ok(message) => message,
                        Err(_) => break,
                    },
                    Some(wait) => match rx.recv_timeout(wait) {
                        Ok(message) => message,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            if self.player.status == State::Playing
                                && self.last_checkpoint.elapsed() >= Duration::from_secs(10)
                                && let Err(error) = self.checkpoint()
                            {
                                (self.emit)(Event::Error(error.to_string()));
                            }
                            continue;
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    },
                };
                match message {
                    Message::Command(Command::RelocateStorage {
                        destination,
                        bootstrap,
                        lease,
                    }) => {
                        if let Err(error) = self.checkpoint() {
                            (self.emit)(Event::Error(format!(
                                "Не удалось сохранить очередь перед переносом: {error}"
                            )));
                            continue;
                        }
                        self = self.move_storage(destination, bootstrap, lease)?;
                        continue 'session;
                    }
                    Message::Command(Command::Shutdown) => {
                        self.downloads.account(String::new());
                        if let Err(error) = self.checkpoint() {
                            (self.emit)(Event::Error(error.to_string()));
                        }
                        break;
                    }
                    Message::Command(command) => {
                        if let Err(e) = self.command(command) {
                            (self.emit)(Event::Error(e.to_string()));
                        }
                    }
                    Message::Engine(generation, event) if generation == self.engine_generation => {
                        self.engine_event(event)
                    }
                    Message::Engine(_, _) => {}
                    Message::NetworkOnline => self.send_flush_likes(true),
                    Message::Bitrate(generation, id, bitrate) => {
                        self.apply_bitrate(generation, id, bitrate);
                    }
                    Message::Source(generation, occurrence, id) => {
                        self.apply_source(generation, occurrence, id);
                    }
                    Message::Catalog(generation, reply) => {
                        let reply = *reply;
                        self.refresh_like_deadline();
                        if generation != 0
                            && generation != self.view_generation
                            && !matches!(
                                &reply,
                                Ok(CatalogReply::PlaylistChanged(..)
                                    | CatalogReply::PlaylistFailed(_)
                                    | CatalogReply::PlaylistEdited(..)
                                    | CatalogReply::RadioResult(..))
                            )
                        {
                            continue;
                        }
                        if generation != 0
                            && generation == self.view_generation
                            && !matches!(
                                &reply,
                                Ok(CatalogReply::PlaylistChanged(..)
                                    | CatalogReply::PlaylistFailed(_)
                                    | CatalogReply::PlaylistEdited(..)
                                    | CatalogReply::RadioResult(..))
                            )
                        {
                            self.busy = false;
                            (self.emit)(Event::Busy(false));
                        }
                        match reply {
                            Ok(CatalogReply::RadioResult(account, start, result)) => {
                                self.radio_finished(account, start, result)
                            }
                            Ok(CatalogReply::Tracks(items, title)) => {
                                let length = Duration::from_millis(
                                    items.iter().map(|r| r.track.duration_ms).sum(),
                                );
                                self.header_job = None;
                                if self.ui_attached
                                    && let Some(first) = items.first()
                                    && let Some(catalog) = &self.catalog
                                {
                                    let _ = catalog.send(CatalogRequest::Cover(
                                        first.track.clone(),
                                        CoverTarget::Collection,
                                        self.view_generation,
                                        self.cover_generation,
                                    ));
                                }
                                self.title = title.clone();
                                self.length = length;
                                self.publish(items);
                                (self.emit)(Event::Collection { title, length });
                            }
                            Ok(CatalogReply::Page {
                                items,
                                title,
                                header,
                                cards,
                                cover,
                                playlist_snapshot,
                            }) => {
                                self.playlist_snapshot = playlist_snapshot;
                                let length = Duration::from_millis(
                                    items.iter().map(|r| r.track.duration_ms).sum(),
                                );
                                self.header_job = cover.clone();
                                match cover {
                                    Some(job) => self.covers.request(vec![job]),
                                    None => {
                                        if self.ui_attached
                                            && let Some(first) = items.first()
                                            && let Some(catalog) = &self.catalog
                                        {
                                            let _ = catalog.send(CatalogRequest::Cover(
                                                first.track.clone(),
                                                CoverTarget::Collection,
                                                self.view_generation,
                                                self.cover_generation,
                                            ));
                                        }
                                    }
                                }
                                self.publish(items);
                                self.cards = cards.clone();
                                self.title = title.clone();
                                self.length = length;
                                (self.emit)(Event::Collection { title, length });
                                self.header = header.clone();
                                (self.emit)(Event::Header(header));
                                (self.emit)(Event::Cards(cards));
                            }
                            Ok(CatalogReply::Cards(cards)) => {
                                self.header_job = None;
                                self.publish(Vec::new());
                                self.cards = cards.clone();
                                (self.emit)(Event::Cards(cards));
                            }
                            Ok(CatalogReply::CollectionLiked(id, liked)) => {
                                if self.header.id == id.to_string() {
                                    self.header.liked = liked;
                                }
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
                            Ok(CatalogReply::PlaylistEdited(snapshot, rows, error)) => {
                                self.playlist_edit_finished(generation, snapshot, rows, error)
                            }
                            Ok(CatalogReply::Collections(collections)) => {
                                self.collections = collections.clone();
                                (self.emit)(Event::Collections(collections))
                            }
                            Ok(CatalogReply::Unchanged) => {}
                            Ok(CatalogReply::LikesPending) => self.send_flush_likes(false),
                            Ok(CatalogReply::Cover(target, id, cover, generation)) => {
                                if self.ui_attached && generation == self.cover_generation {
                                    (self.emit)(Event::Cover(target, id, cover));
                                }
                            }
                            Ok(CatalogReply::Account(changed)) => {
                                if changed {
                                    self.reset_account_view()?;
                                }
                                self.downloads.account(self.account.clone());
                                (self.emit)(Event::Account);
                                (self.emit)(Event::Providers(self.providers.statuses()));
                                self.background(Command::SyncCollections)?;
                                self.background(Command::SyncLibrary)?;
                            }
                            Ok(CatalogReply::Providers) => {
                                (self.emit)(Event::Providers(self.providers.statuses()));
                                self.background(Command::SyncCollections)?;
                                self.background(Command::SyncLibrary)?;
                            }
                            Ok(reply @ (CatalogReply::Synced | CatalogReply::SyncedOffer(..))) => {
                                let liked = self
                                    .store
                                    .liked_track_ids()?
                                    .into_iter()
                                    .filter_map(|id| id.parse().ok())
                                    .collect::<HashSet<TrackId>>();
                                self.queue_likes = liked;
                                let mut rows = self.rows.clone();
                                for row in &mut rows {
                                    row.liked = self.queue_likes.contains(&row.track.id);
                                }
                                self.publish(rows);
                                let player_liked = self
                                    .player
                                    .track
                                    .as_ref()
                                    .is_some_and(|track| self.queue_likes.contains(&track.id));
                                Arc::make_mut(&mut self.player).liked = player_liked;
                                (self.emit)(Event::Player(self.player.clone()));
                                if self.view == View::Library {
                                    // View-bound, so a reply racing a navigation is discarded.
                                    self.request(Command::Library)?;
                                }
                                if let CatalogReply::SyncedOffer(provider, count) = reply {
                                    (self.emit)(Event::LikesBackfillOffer { provider, count });
                                }
                            }
                            Ok(CatalogReply::AlbumsSynced) => {
                                if self.view == View::Albums {
                                    self.request(Command::Albums)?;
                                }
                            }
                            Ok(CatalogReply::ArtistsSynced) => {
                                if self.view == View::Artists {
                                    self.request(Command::Artists)?;
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
            return Ok(());
        }
    }
    fn move_storage(
        mut self,
        destination: PathBuf,
        bootstrap: PathBuf,
        lease: Option<Arc<eagle_store::relocation::StorageLease>>,
    ) -> Result<Self> {
        let clock = self.clock.clone();
        (self.emit)(Event::StorageMoving(true));
        self.stop_engine();
        self.catalog.take();
        if let Some(worker) = self.catalog_worker.take() {
            let _ = worker.join();
        }
        let root = self
            .cache
            .root()
            .parent()
            .ok_or("missing managed root")?
            .to_owned();
        let next_engine_generation = self.engine_generation.wrapping_add(1);
        let next_view_generation = self.view_generation.wrapping_add(1);
        let next_cover_generation = self.cover_generation.wrapping_add(1);
        let ui_attached = self.ui_attached;
        let playlist_edit_busy = self.playlist_edit_busy;
        let playlist_edit_target = self.playlist_edit_target.clone();
        let playlist_edit_notice = self.playlist_edit_notice.clone();
        let radio = self.radio.clone();
        let radio_busy = self.radio_busy;
        let radio_can_more = self.radio_can_more;
        let radio_notice = self.radio_notice.clone();
        let Self {
            store,
            cache,
            downloads,
            covers,
            emit,
            providers,
            tx,
            ..
        } = self;
        drop(downloads);
        drop(covers);
        let outcome = crate::storage::relocate(
            store,
            cache,
            &root,
            &destination,
            &bootstrap,
            lease.as_deref(),
        )?;
        let mut next = match outcome {
            crate::storage::MoveOutcome::Busy { store, cache } => {
                (emit)(Event::Error(
                    "Хранилище используется; повторите перенос после остановки воспроизведения"
                        .into(),
                ));
                Self::new_with_handles(
                    root,
                    emit.clone(),
                    tx,
                    Some(providers.all().to_vec()),
                    providers.tokens().clone(),
                    Some((store, cache)),
                    clock.clone(),
                )?
            }
            crate::storage::MoveOutcome::Reopen { root, error } => {
                if let Some(error) = error {
                    (emit)(Event::Error(format!("Перенос не завершён: {error}")));
                }
                Self::new_with_providers(
                    root,
                    emit.clone(),
                    tx,
                    Some(providers.all().to_vec()),
                    providers.tokens().clone(),
                    clock,
                )?
            }
        };
        next.engine_generation = next_engine_generation;
        next.view_generation = next_view_generation;
        next.cover_generation = next_cover_generation;
        next.ui_attached = ui_attached;
        next.playlist_edit_busy = playlist_edit_busy;
        next.playlist_edit_target = playlist_edit_target;
        next.playlist_edit_notice = playlist_edit_notice;
        next.radio = radio;
        next.radio_busy = radio_busy;
        next.radio_can_more = radio_can_more;
        next.radio_notice = radio_notice;
        next.covers.set_active(ui_attached);
        (emit)(Event::StorageMoving(false));
        next.storage()?;
        Ok(next)
    }
    fn refresh_local_access(&mut self) -> Result<()> {
        let mut rows = self.rows.clone();
        for row in &mut rows {
            let access = self
                .store
                .access(&self.account, &row.track.id.to_string())?;
            row.blocked_reason = if access.allows_playback() {
                None
            } else {
                Some(
                    access
                        .reason
                        .unwrap_or_else(|| "Доступ отозван провайдером".into()),
                )
            };
        }
        self.publish(rows);
        if let Some(track) = &self.player.track
            && !self
                .store
                .access(&self.account, &track.id.to_string())?
                .allows_playback()
            && (self.engine.is_some() || self.player.status != State::Paused)
        {
            let now = Instant::now();
            if let Some(anchor) = &mut self.last_position {
                anchor.position = position_now(anchor, now);
                anchor.audible_at = now;
                anchor.state = State::Paused;
                self.restored_position = anchor.position;
            }
            self.stop_engine();
            Arc::make_mut(&mut self.player).status = State::Paused;
            (self.emit)(Event::Player(self.player.clone()));
            if let Some(anchor) = &self.last_position {
                (self.emit)(Event::Position(anchor.clone()));
            }
            (self.emit)(Event::Error(
                "Провайдер подтвердил потерю доступа к треку".into(),
            ));
            self.checkpoint()?;
        }
        Ok(())
    }
    /// The account that owns `track`: this worker's account when the track comes from its
    /// provider, otherwise none.
    fn account_of(&self, track: &TrackId) -> &str {
        match self.providers.account_provider() {
            Some(provider) if provider.info().id == track.provider() => &self.account,
            _ => "",
        }
    }
    /// Forgets one provider's session. Its likes and playlists stay; offline saves of the
    /// account that owns them are removed unless `keep_saved`.
    fn logout(&mut self, provider_id: &str, keep_saved: bool) -> Result<()> {
        let provider = self
            .providers
            .get(provider_id)
            .ok_or("провайдер недоступен")?
            .clone();
        let id = provider.info().id;
        let owns_account = self.providers.account_provider().map(|p| p.info().id) == Some(id);
        let mut failure = None;
        if owns_account && !keep_saved {
            for job in self.store.downloads(&self.account)? {
                if job.state == DownloadState::Saved
                    && job.track.provider() == id
                    && let Err(error) = self.cache.remove_saved(&self.store, &self.account, job.id)
                {
                    failure.get_or_insert(error);
                }
            }
        }
        self.providers.sign_out(provider.as_ref())?;
        let account_setting = account_key(id);
        self.store.write(move |c| {
            c.execute("DELETE FROM settings WHERE key=?1", [account_setting])?;
            Ok(())
        })?;
        if owns_account {
            self.reset_account_view()?;
            (self.emit)(Event::Downloads(self.store.downloads(&self.account)?));
            (self.emit)(Event::Snapshot(Box::new(self.snapshot())));
        }
        (self.emit)(Event::Providers(self.providers.statuses()));
        self.storage()?;
        match failure {
            Some(error) => Err(error.into()),
            None => Ok(()),
        }
    }
    fn reset_account_view(&mut self) -> Result<()> {
        self.radio = None;
        self.radio_can_more = false;
        self.radio_notice = None;
        let account = stored_account(&self.store, &self.providers)?;
        // Radio sessions belong to an account, so their contexts are dropped before the
        // checkpoint; the queue and position belong to the profile and are restored as they were.
        self.queue_radio
            .retain(|_, context| context.account == account);
        self.checkpoint()?;
        self.stop_engine();
        self.queue.clear();
        self.queue_original.clear();
        self.queue_ids.clear();
        self.queue_radio.clear();
        self.engine_queue.clear();
        self.current = 0;
        self.restored_position = Duration::ZERO;
        self.last_position = None;
        self.account = account;
        self.downloads.account(self.account.clone());
        self.player = Arc::default();
        self.restore_playback()?;
        self.emit_queue();
        self.collections.clear();
        self.cards.clear();
        self.header = Header::default();
        self.header_job = None;
        self.title.clear();
        self.length = Duration::ZERO;
        self.publish(Vec::new());
        (self.emit)(Event::Cards(Vec::new()));
        (self.emit)(Event::Header(Header::default()));
        self.queue_likes.clear();
        Arc::make_mut(&mut self.player).liked = false;
        (self.emit)(Event::Player(self.player.clone()));
        let mut rows = self.rows.clone();
        for row in &mut rows {
            row.liked = false;
        }
        self.publish(rows);
        (self.emit)(Event::Collections(Vec::new()));
        match self.view {
            View::Library => self.request(Command::Library)?,
            View::Albums => self.request(Command::Albums)?,
            View::Artists => self.request(Command::Artists)?,
            View::Album | View::Artist => {
                (self.emit)(Event::Header(Header::default()));
            }
            View::Playlist | View::Saved | View::History => {
                self.command(Command::Open(View::Library))?;
            }
            _ => {}
        }
        Ok(())
    }
    fn playlist_edit_finished(
        &mut self,
        generation: u64,
        snapshot: Option<PlaylistSnapshot>,
        rows: Option<Vec<LibraryItem>>,
        error: Option<String>,
    ) {
        self.playlist_edit_busy = false;
        let target = self.playlist_edit_target.take();
        let visible = self.view == View::Playlist && target.as_ref() == self.playlist_id.as_ref();
        if visible {
            (self.emit)(Event::PlaylistMutationBusy(false));
        }
        if generation == self.view_generation && self.view == View::Playlist {
            if let (Some(snapshot), Some(rows)) = (snapshot, rows)
                && self.playlist_id.as_ref() == Some(&snapshot.playlist.id)
            {
                self.header.playlist_revision = Some(snapshot.revision);
                self.playlist_snapshot = Some(snapshot);
                self.publish(rows);
            } else {
                self.playlist_snapshot = None;
                self.header.editable = false;
                self.header.playlist_revision = None;
            }
            (self.emit)(Event::Header(self.header.clone()));
        }
        if let Some(error) = error {
            if let Some(id) = target {
                self.playlist_edit_notice = Some((id, error.clone()));
            }
            if visible {
                (self.emit)(Event::PlaylistMutationFailed(error));
            }
        }
    }
    fn playlist_changed(
        &mut self,
        generation: u64,
        collections: Vec<Collection>,
        created: Option<Playlist>,
        deleted: Option<TrackId>,
    ) -> Result<()> {
        (self.emit)(Event::PlaylistMutationBusy(false));
        self.collections = collections.clone();
        (self.emit)(Event::Collections(collections));
        if let Some(playlist) = created {
            (self.emit)(Event::PlaylistCreated(playlist.id.clone()));
            if generation == self.view_generation {
                self.command(Command::Playlist(playlist.id))?;
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
        let updates = self
            .rows
            .iter_mut()
            .enumerate()
            .filter_map(|(index, row)| {
                if row.track.id != id {
                    return None;
                }
                row.liked = liked;
                Some(LibraryDelta::Update {
                    index,
                    item: row.clone(),
                })
            })
            .collect::<Vec<_>>();
        if !updates.is_empty() {
            (self.emit)(Event::Library(updates));
        }
    }
    /// Maps UI keys to cover jobs using what is currently listed.
    fn thumbnails(&self, keys: Vec<String>) {
        if !self.ui_attached {
            return;
        }
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
    fn radio_finished(
        &mut self,
        account: String,
        start: bool,
        result: std::result::Result<RadioSession, RadioError>,
    ) {
        self.radio_busy = false;
        if account == self.account {
            match result {
                Ok(session) => {
                    let refill = !start
                        && self.radio.as_ref().is_some_and(|(owner, current, _)| {
                            owner == &account && current.id == session.id
                        });
                    let append_error = if refill {
                        self.append_radio_batch(&account, &session.id, &session.batch)
                            .err()
                    } else {
                        None
                    };
                    if start {
                        self.radio = Some((account, session.clone(), vec![session.batch.clone()]));
                    } else if let Some((_, current, batches)) = &mut self.radio
                        && current.id == session.id
                    {
                        batches.push(session.batch.clone());
                        *current = session;
                    }
                    self.radio_can_more = true;
                    self.radio_notice = append_error.map(|error| {
                        format!("Рекомендации получены, но очередь пополнена не полностью: {error}")
                    });
                }
                Err(error) => {
                    if matches!(error, RadioError::UnknownSession | RadioError::Uncertain) {
                        self.radio_can_more = false;
                    }
                    self.radio_notice = Some(error.to_string());
                }
            }
            if self.view == View::Radio {
                self.publish_radio();
            }
        }
        self.emit_radio_state();
    }
    fn append_radio_batch(
        &mut self,
        account: &str,
        session_id: &str,
        batch: &RadioBatch,
    ) -> Result<()> {
        let Some(current) = self.queue.get(self.current) else {
            return Ok(());
        };
        if !self
            .queue_radio
            .get(&self.occurrence_id(current))
            .is_some_and(|context| context.account == account && context.session_id == session_id)
        {
            return Ok(());
        }
        let tracks: Vec<_> = batch
            .tracks
            .iter()
            .filter(|track| track.availability == Availability::Available)
            .map(|track| Arc::new(track.clone()))
            .collect();
        let count = u64::try_from(tracks.len()).map_err(|_| "слишком много треков")?;
        self.next_queue_id
            .checked_add(count)
            .ok_or("исчерпаны идентификаторы очереди")?;
        let mut prepared = Vec::with_capacity(tracks.len());
        for track in tracks {
            let id = self.next_queue_id;
            self.next_queue_id += 1;
            self.queue_ids.insert(Arc::as_ptr(&track) as usize, id);
            let source = if self.engine.is_some() {
                match self.source(&track) {
                    Ok(source) => Some(source),
                    Err(error) => {
                        self.queue_ids.remove(&(Arc::as_ptr(&track) as usize));
                        for (previous, _, _) in prepared {
                            self.queue_ids.remove(&(Arc::as_ptr(&previous) as usize));
                        }
                        return Err(error);
                    }
                }
            } else {
                None
            };
            prepared.push((track, id, source));
        }
        let mut failure = None;
        for (track, id, source) in prepared {
            if failure.is_none()
                && let (Some(engine), Some(source)) = (&self.engine, source)
                && let Err(error) = engine.enqueue(source)
            {
                failure = Some(error.to_string());
            }
            if failure.is_some() {
                self.queue_ids.remove(&(Arc::as_ptr(&track) as usize));
                continue;
            }
            self.queue_radio.insert(
                id,
                eagle_store::RadioContext {
                    account: account.to_owned(),
                    session_id: session_id.to_owned(),
                    batch_id: batch.id.clone(),
                },
            );
            self.queue.push(track.clone());
            self.queue_original.push(track.clone());
            if self.engine.is_some() {
                self.engine_queue.push(track);
            }
        }
        if let Err(error) = self.configure_playback() {
            failure = Some(error.to_string());
        }
        self.emit_queue();
        self.checkpoint()?;
        if let Some(error) = failure {
            return Err(error.into());
        }
        Ok(())
    }
    fn emit_radio_state(&self) {
        (self.emit)(Event::RadioState {
            ready: self.radio_can_more && !self.radio_busy,
            busy: self.radio_busy,
            notice: self.radio_notice.clone(),
        });
    }
    fn publish_radio(&mut self) {
        let rows = self
            .radio
            .as_ref()
            .filter(|(account, _, _)| account == &self.account)
            .map(|(_, _, batches)| {
                batches
                    .iter()
                    .flat_map(|batch| batch.tracks.iter())
                    .map(|track| LibraryItem {
                        sources: Vec::new(),
                        track: Arc::new(track.clone()),
                        liked: self.queue_likes.contains(&track.id),
                        blocked_reason: None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.publish(rows);
        self.title = "Моя волна".into();
        (self.emit)(Event::Collection {
            title: self.title.clone(),
            length: Duration::ZERO,
        });
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
    /// Asks the catalog to send the due likes. Until its reply, no like deadline is armed.
    fn send_flush_likes(&mut self, online: bool) {
        self.like_retry_at = None;
        if let Some(catalog) = &self.catalog {
            let _ = catalog.send(CatalogRequest::FlushLikes { online });
        }
    }
    /// Reads the earliest future like retry from the store. Called after every catalog reply,
    /// which is the only place where likes are sent or deferred.
    fn refresh_like_deadline(&mut self) {
        let now = (self.clock)();
        self.like_retry_at = match self.store.next_like_retry_at(now) {
            Ok(at) => at,
            Err(error) => {
                log::warn!(error = error.to_string().as_str(); "like retry time unreadable");
                None
            }
        };
    }
    fn like_due(&self) -> bool {
        self.like_retry_at.is_some_and(|at| (self.clock)() >= at)
    }
    fn like_wait(&self) -> Option<Duration> {
        self.like_retry_at
            .map(|at| Duration::from_secs((at - (self.clock)()).max(0) as u64))
    }
    /// How long the loop may block: until the playback checkpoint or the next like retry.
    /// `None` when neither exists, so an idle worker waits without a timer.
    fn timer_wait(&self) -> Option<Duration> {
        let checkpoint = (self.player.status == State::Playing)
            .then(|| Duration::from_secs(10).saturating_sub(self.last_checkpoint.elapsed()));
        [checkpoint, self.like_wait()].into_iter().flatten().min()
    }
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
            self.header_job = None;
            self.playlist_snapshot = None;
            self.busy = true;
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
        let checkpoint = matches!(
            &command,
            Command::Play(_)
                | Command::PlayRow(_)
                | Command::PlayAll(_)
                | Command::Pause
                | Command::Resume
                | Command::TogglePlay
                | Command::Stop
                | Command::Previous
                | Command::Seek(_)
                | Command::Volume(_)
                | Command::ToggleShuffle
                | Command::SetShuffle(_)
                | Command::CycleRepeat
                | Command::MoveQueue { .. }
                | Command::ShiftQueue { .. }
                | Command::PlayNext(_)
                | Command::EnqueueNext(_)
                | Command::Next
        );
        if let Command::Login { provider, .. } = &command
            && self.providers.account_provider().map(|p| p.info().id) == Some(provider.as_str())
        {
            self.downloads.account(String::new());
        }
        match command {
            Command::RadioStart | Command::RadioMore => {
                if self.view != View::Radio {
                    return Err("откройте Мою волну".into());
                }
                if self.radio_busy {
                    return Err("запрос радио уже выполняется".into());
                }
                if self.account.is_empty() {
                    return Err("войдите в аккаунт для радио".into());
                }
                let request = if matches!(command, Command::RadioStart) {
                    CatalogRequest::RadioStart(self.view_generation, self.account.clone())
                } else {
                    let (account, session, batches) = self
                        .radio
                        .as_ref()
                        .filter(|(account, _, _)| account == &self.account && self.radio_can_more)
                        .ok_or("начните новую радиосессию")?;
                    let queue = batches
                        .iter()
                        .flat_map(|batch| batch.tracks.iter())
                        .map(|track| RadioQueueItem {
                            track: track.id.clone(),
                            album: track.album.clone(),
                        })
                        .collect();
                    CatalogRequest::RadioMore(
                        self.view_generation,
                        account.clone(),
                        session.clone(),
                        queue,
                    )
                };
                if self
                    .catalog
                    .as_ref()
                    .is_some_and(|catalog| catalog.send(request).is_ok())
                {
                    self.radio_busy = true;
                    self.radio_notice = None;
                    self.emit_radio_state();
                } else {
                    self.radio_notice = Some("каталог остановлен".into());
                    self.emit_radio_state();
                }
            }
            Command::ClearPlaylistMutationNotice(id) => {
                if self
                    .playlist_edit_notice
                    .as_ref()
                    .is_some_and(|(target, _)| target == &id)
                {
                    self.playlist_edit_notice = None;
                }
            }
            Command::AddTrackToPlaylist {
                id,
                revision,
                entry,
            } => {
                let at = self
                    .playlist_snapshot
                    .as_ref()
                    .ok_or("плейлист недоступен для изменения")?
                    .playlist
                    .tracks
                    .len();
                self.command(Command::EditPlaylist {
                    id,
                    revision,
                    edit: PlaylistEdit::Insert {
                        at,
                        tracks: vec![*entry],
                    },
                })?;
            }
            Command::EditPlaylist { id, revision, edit } => {
                if self.playlist_edit_busy {
                    return Err("изменение плейлиста уже выполняется".into());
                }
                let snapshot = self
                    .playlist_snapshot
                    .clone()
                    .filter(|_| self.view == View::Playlist && self.header.editable)
                    .ok_or("плейлист недоступен для изменения")?;
                if snapshot.playlist.id != id {
                    return Err("открыт другой плейлист".into());
                }
                let target = id.clone();
                let request = if snapshot.revision == revision {
                    CatalogRequest::EditPlaylist(self.view_generation, snapshot, edit)
                } else {
                    CatalogRequest::RefreshPlaylist(self.view_generation, id)
                };
                let sent = self
                    .catalog
                    .as_ref()
                    .is_some_and(|catalog| catalog.send(request).is_ok());
                if sent {
                    self.playlist_edit_busy = true;
                    self.playlist_edit_target = Some(target);
                    self.playlist_edit_notice = None;
                    (self.emit)(Event::PlaylistMutationBusy(true));
                } else {
                    (self.emit)(Event::PlaylistMutationFailed("каталог остановлен".into()));
                }
            }
            Command::UiAttached(attached) => {
                if self.ui_attached != attached {
                    self.cover_generation = self.cover_generation.wrapping_add(1);
                }
                self.ui_attached = attached;
                self.covers.set_active(attached);
                if !attached && let Some(catalog) = &self.catalog {
                    let _ = catalog.send(CatalogRequest::ClearCovers);
                }
            }
            Command::RemoveSaved(id) => {
                let result = self.cache.remove_saved(&self.store, &self.account, id);
                (self.emit)(Event::Downloads(self.store.downloads(&self.account)?));
                self.storage()?;
                result?;
            }
            Command::CacheLimit(limit) => {
                if !(256 * 1024 * 1024..=128 * 1024 * 1024 * 1024).contains(&limit) {
                    return Err("неверный лимит кэша".into());
                }
                self.store.put_setting("cache_limit", limit.to_string())?;
                self.cache.set_limit(limit);
                self.storage()?;
            }
            Command::AllowMetered(allow) => {
                self.store.put_setting("allow_metered", allow.to_string())?;
                self.network_policy()?;
            }
            Command::Downloads => {
                self.refresh_local_access()?;
                self.network_policy()?;
                self.storage()?;
                (self.emit)(Event::Downloads(self.store.downloads(&self.account)?));
            }
            Command::SaveAll(quality) => {
                if self.account.is_empty() {
                    return Err("войдите в аккаунт перед сохранением".into());
                }
                let tracks = self
                    .rows
                    .iter()
                    .map(|row| row.track.id.clone())
                    .collect::<Vec<_>>();
                self.store
                    .enqueue_downloads(&self.account, &tracks, quality)?;
                self.downloads.wake();
            }
            Command::Save { id, quality } => {
                if self.account.is_empty() {
                    return Err("войдите в аккаунт перед сохранением".into());
                }
                self.store.enqueue_download(&self.account, &id, quality)?;
                self.downloads.wake();
            }
            Command::PauseDownload(id)
            | Command::CancelDownload(id)
            | Command::RetryDownload(id) => {
                let state = match command {
                    Command::PauseDownload(_) => eagle_store::DownloadState::Paused,
                    Command::CancelDownload(_) => eagle_store::DownloadState::Cancelled,
                    _ => eagle_store::DownloadState::Queued,
                };
                self.store.control_download(&self.account, id, state)?;
                self.downloads.wake();
            }
            Command::Snapshot => {
                (self.emit)(Event::Snapshot(Box::new(self.snapshot())));
                self.storage()?;
                self.network_policy()?;
                (self.emit)(Event::Downloads(self.store.downloads(&self.account)?));
                if self.ui_attached
                    && let Some(track) = &self.player.track
                    && let Some(catalog) = &self.catalog
                {
                    let _ = catalog.send(CatalogRequest::Cover(
                        track.clone(),
                        CoverTarget::Player,
                        0,
                        self.cover_generation,
                    ));
                }
                if self.ui_attached {
                    if let Some(job) = self.header_job.clone() {
                        self.covers.request(vec![job]);
                    } else if let Some(first) = self.rows.first()
                        && let Some(catalog) = &self.catalog
                    {
                        let _ = catalog.send(CatalogRequest::Cover(
                            first.track.clone(),
                            CoverTarget::Collection,
                            self.view_generation,
                            self.cover_generation,
                        ));
                    }
                }
            }
            Command::EnqueueNext(id) => {
                if self.view == View::Radio {
                    return Err("добавление из радио пока недоступно".into());
                }
                let track = self
                    .rows
                    .iter()
                    .map(|row| row.track.as_ref())
                    .find(|t| t.id == id)
                    .cloned()
                    .or(self.store.track(&id)?)
                    .ok_or("метаданные трека не найдены")?;
                if track.availability != Availability::Available {
                    return Err("трек недоступен".into());
                }
                let track = Arc::new(track);
                let occurrence = self.next_queue_id;
                self.next_queue_id = occurrence
                    .checked_add(1)
                    .ok_or("исчерпаны идентификаторы очереди")?;
                let key = Arc::as_ptr(&track) as usize;
                self.queue_ids.insert(key, occurrence);
                if let Some(engine) = &self.engine {
                    let result = self.source(&track).and_then(|source| {
                        engine.enqueue(source)?;
                        Ok(())
                    });
                    if let Err(error) = result {
                        self.queue_ids.remove(&key);
                        return Err(error);
                    }
                    self.engine_queue.push(track.clone());
                }
                let next = if self.queue.is_empty() {
                    0
                } else {
                    self.current + 1
                };
                self.queue.insert(next, track.clone());
                self.queue_original.push(track.clone());
                self.assign_occurrences()?;
                if self.player.track.is_none() {
                    let player = Arc::make_mut(&mut self.player);
                    player.track = Some(track);
                    player.status = State::Paused;
                    self.current = 0;
                    (self.emit)(Event::Player(self.player.clone()));
                }
                self.configure_playback()?;
                self.emit_queue();
            }
            Command::MoveQueue { id, to } => self.move_queue(id, to)?,
            Command::ShiftQueue { id, delta } => {
                let from = self
                    .queue
                    .iter()
                    .position(|track| self.occurrence_id(track) == id)
                    .ok_or("элемент очереди не найден")?;
                let to = (from as i64 + i64::from(delta))
                    .clamp(0, self.queue.len().saturating_sub(1) as i64)
                    as usize;
                self.move_queue(id, to)?;
            }
            Command::PlayNext(id) => {
                let from = self
                    .queue
                    .iter()
                    .position(|track| self.occurrence_id(track) == id)
                    .ok_or("элемент очереди не найден")?;
                if from != self.current {
                    let to = if from < self.current {
                        self.current
                    } else {
                        self.current + 1
                    };
                    self.move_queue(id, to)?;
                }
            }
            Command::Open(view) => {
                self.playlist_id = None;
                self.playlist_snapshot = None;
                self.title.clear();
                self.length = Duration::ZERO;
                self.header = Header::default();
                self.header_job = None;
                self.busy = false;
                self.view = view;
                (self.emit)(Event::View(view));
                self.view_generation += 1;
                (self.emit)(Event::Busy(false));
                self.cards.clear();
                match view {
                    View::Saved | View::History => {
                        let tracks = if view == View::Saved {
                            self.store.saved_tracks(&self.account)?
                        } else {
                            self.store.history(&self.profile)?
                        };
                        let rows =
                            tracks
                                .into_iter()
                                .map(|track| {
                                    let access =
                                        self.store.access(&self.account, &track.id.to_string())?;
                                    Ok(LibraryItem {
                                        sources: Vec::new(),
                                        blocked_reason: if access.allows_playback() {
                                            None
                                        } else {
                                            Some(access.reason.unwrap_or_else(|| {
                                                "Доступ отозван провайдером".into()
                                            }))
                                        },
                                        track: Arc::new(track),
                                        liked: false,
                                    })
                                })
                                .collect::<Result<Vec<_>>>()?;
                        self.title = if view == View::Saved {
                            "Сохранённое"
                        } else {
                            "История на этом устройстве"
                        }
                        .to_owned();
                        self.publish(rows);
                        (self.emit)(Event::Collection {
                            title: self.title.clone(),
                            length: Duration::ZERO,
                        });
                    }
                    View::Library => self.request(Command::Library)?,
                    View::Albums => self.request(Command::Albums)?,
                    View::Artists => self.request(Command::Artists)?,
                    View::Search | View::Album | View::Playlist | View::Artist => {
                        self.publish(Vec::new())
                    }
                    View::Login | View::Settings => {}
                    View::Radio => {
                        self.publish_radio();
                        self.emit_radio_state();
                    }
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
                self.title.clear();
                self.header = Header::default();
                self.header_job = None;
                self.length = Duration::ZERO;
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
                    self.busy = false;
                    (self.emit)(Event::Busy(false));
                } else {
                    self.request(command)?;
                }
            }
            command @ (Command::Library
            | Command::Collections
            | Command::Login { .. }
            | Command::LikesBackfill { .. }
            | Command::Like(_)) => self.request(command)?,
            Command::Logout {
                provider,
                keep_saved,
            } => self.logout(&provider, keep_saved)?,
            Command::Album(id) => {
                self.title.clear();
                self.header = Header::default();
                self.header_job = None;
                self.length = Duration::ZERO;
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
                use std::hash::{BuildHasher, RandomState};
                let random = RandomState::new();
                if let Some(index) =
                    select_play_all(&self.rows, shuffle, |index| random.hash_one(index))
                {
                    self.play_row(index)?;
                }
            }
            Command::Play(id) => {
                if self.view == View::Radio {
                    return Err("воспроизведение радио пока недоступно".into());
                }
                let index = self
                    .rows
                    .iter()
                    .position(|r| r.track.id == id)
                    .ok_or("трек не найден в текущем списке")?;
                self.play_row(index)?;
            }
            Command::PlayRow(index) => self.play_row(index)?,
            Command::Next => {
                if let Some(engine) = &self.engine {
                    engine.next()?;
                } else if self.current + 1 < self.queue.len() {
                    self.select_paused(self.current + 1);
                } else if self.player.repeat == Repeat::All && !self.queue.is_empty() {
                    self.select_paused(0);
                }
            }
            Command::Previous => {
                let paused = self.player.status == State::Paused;
                self.current = self.current.saturating_sub(1);
                if self.engine.is_none() {
                    self.select_paused(self.current);
                } else {
                    self.play_current()?;
                }
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
                    let position = self.restored_position;
                    self.play_current()?;
                    if let Some(engine) = &self.engine {
                        engine.seek(position)?;
                    }
                    self.restored_position = position;
                    self.last_position = Some(PositionAnchor {
                        track: 0,
                        position,
                        audible_at: Instant::now(),
                        state: State::Loading,
                    });
                }
            }
            Command::Stop => {
                self.stop_engine();
                self.restored_position = Duration::ZERO;
                self.last_position = None;
                Arc::make_mut(&mut self.player).status = State::Idle;
                (self.emit)(Event::Player(self.player.clone()));
            }
            Command::TogglePlay => {
                if self.engine.is_none() {
                    self.command(Command::Resume)?;
                } else if self.player.status == State::Idle {
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
                self.restored_position = to;
                self.last_position = Some(PositionAnchor {
                    track: 0,
                    position: to,
                    audible_at: Instant::now(),
                    state: self.player.status,
                });
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
                    self.queue[self.current + 1..]
                        .sort_by_cached_key(|t| random.hash_one(Arc::as_ptr(t)));
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
            Command::RelocateStorage { .. } | Command::Shutdown => {}
        }
        if checkpoint {
            self.checkpoint()?;
        }
        Ok(())
    }
    fn play_row(&mut self, index: usize) -> Result<()> {
        self.prepare_row(index)?;
        self.play_current()
    }
    fn prepare_row(&mut self, index: usize) -> Result<()> {
        let (queue, selected) = initial_queue(&self.rows, index)?;
        let origins: Vec<_> = if self.view == View::Radio {
            let (account, session, batches) =
                self.radio.as_ref().ok_or("радиосессия недоступна")?;
            if account != &self.account {
                return Err("радиосессия другого аккаунта".into());
            }
            let all: Vec<_> = batches
                .iter()
                .flat_map(|batch| {
                    batch.tracks.iter().map(|_| eagle_store::RadioContext {
                        account: account.clone(),
                        session_id: session.id.clone(),
                        batch_id: batch.id.clone(),
                    })
                })
                .collect();
            if all.len() != self.rows.len() {
                return Err("рекомендации радио изменились; обновите список".into());
            }
            self.rows
                .iter()
                .zip(all)
                .filter(|(row, _)| {
                    row.blocked_reason.is_none()
                        && row.track.availability == Availability::Available
                })
                .map(|(_, context)| context)
                .collect()
        } else {
            Vec::new()
        };
        self.restored_position = Duration::ZERO;
        self.queue = queue;
        self.queue_ids.clear();
        self.queue_radio.clear();
        self.queue_original = self.queue.clone();
        self.assign_occurrences()?;
        for (track, context) in self.queue_original.iter().zip(origins) {
            self.queue_radio.insert(self.occurrence_id(track), context);
        }
        let track = self.queue[selected].clone();
        self.queue_likes = self
            .rows
            .iter()
            .filter(|r| r.liked)
            .map(|r| r.track.id.clone())
            .collect();
        if self.player.shuffle {
            use std::hash::{BuildHasher, RandomState};
            self.queue.remove(selected);
            let random = RandomState::new();
            self.queue
                .sort_by_cached_key(|t| random.hash_one(Arc::as_ptr(t)));
            self.queue.insert(0, track);
            self.current = 0;
        } else {
            self.current = selected;
        }
        Ok(())
    }
    fn select_paused(&mut self, index: usize) {
        self.current = index;
        self.restored_position = Duration::ZERO;
        self.last_position = Some(PositionAnchor {
            track: 0,
            position: Duration::ZERO,
            audible_at: Instant::now(),
            state: State::Paused,
        });
        let player = Arc::make_mut(&mut self.player);
        player.track = self.queue.get(index).cloned();
        player.bitrate_kbps = None;
        player.liked = player
            .track
            .as_ref()
            .is_some_and(|track| self.queue_likes.contains(&track.id));
        player.status = State::Paused;
        (self.emit)(Event::Player(self.player.clone()));
        if let Some(position) = &self.last_position {
            (self.emit)(Event::Position(position.clone()));
        }
        self.emit_queue();
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
        (self.emit)(Event::QueueOrder {
            entries: self
                .queue
                .iter()
                .map(|track| (self.occurrence_id(track), track.clone()))
                .collect(),
            current: self
                .queue
                .get(self.current)
                .map(|track| self.occurrence_id(track)),
        });
        (self.emit)(Event::Queue(upcoming(
            &self.queue,
            self.current,
            self.player.repeat,
        )));
    }
    fn apply_source(&mut self, generation: u64, occurrence: u64, id: TrackId) {
        if generation != self.engine_generation {
            return;
        }
        let provider = id.provider().to_owned();
        self.audio_sources.insert(occurrence, provider.clone());
        if self
            .player
            .track
            .as_ref()
            .is_some_and(|track| self.occurrence_id(track) == occurrence)
            && self.player.source_provider.as_deref() != Some(provider.as_str())
        {
            Arc::make_mut(&mut self.player).source_provider = Some(provider);
            (self.emit)(Event::Player(self.player.clone()));
        }
    }
    fn apply_bitrate(&mut self, generation: u64, occurrence: u64, bitrate: Option<u32>) {
        if generation != self.engine_generation {
            return;
        }
        self.audio_bitrates.insert(occurrence, bitrate);
        if self
            .player
            .track
            .as_ref()
            .is_some_and(|track| self.occurrence_id(track) == occurrence)
            && self.player.bitrate_kbps != bitrate
        {
            Arc::make_mut(&mut self.player).bitrate_kbps = bitrate;
            (self.emit)(Event::Player(self.player.clone()));
        }
    }
    fn source(&self, track: &Arc<Track>) -> Result<eagle_engine::Source> {
        if self.providers.for_track(&track.id).is_none() {
            return Err("провайдер недоступен".into());
        }
        let bitrate_tx = self.tx.clone();
        let source_tx = self.tx.clone();
        let generation = self.engine_generation;
        let occurrence = self.occurrence_id(track);
        Ok(crate::source::lazy_source(crate::source::SourceContext {
            providers: self.providers.clone(),
            store: self.store.clone(),
            cache: self.cache.clone(),
            net: self.net.clone(),
            account_provider: self
                .providers
                .account_provider()
                .map(|p| p.info().id.to_owned()),
            account: self.account.clone(),
            requested: track.id.clone(),
            on_bitrate: Arc::new(move |bitrate| {
                let _ = bitrate_tx.send(Message::Bitrate(generation, occurrence, bitrate));
            }),
            on_source: Arc::new(move |id| {
                let _ = source_tx.send(Message::Source(generation, occurrence, id.clone()));
            }),
        }))
    }
    fn play_current(&mut self) -> Result<()> {
        let Some(track) = self.queue.get(self.current).cloned() else {
            return Ok(());
        };
        log::info!(target: "eagle_app", provider = track.id.provider(), track_id = track.id.local(); "playback");
        self.stop_engine();
        self.history_occurrence = None;
        self.history_marker = None;
        self.history_recorded = false;
        self.audio_bitrates.clear();
        self.audio_sources.clear();
        self.last_position = None;
        self.restored_position = Duration::ZERO;
        let mut engine = Engine::new(Config::default())?;
        engine.volume(self.player.volume)?;
        self.engine_queue = self.queue[self.current..]
            .iter()
            .chain(&self.queue[..self.current])
            .cloned()
            .collect();
        self.engine_generation = self.engine_generation.wrapping_add(1);
        for item in &self.engine_queue {
            engine.enqueue(self.source(item)?)?;
        }
        let events = engine.take_events();
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
        state.bitrate_kbps = None;
        state.status = State::Loading;
        if self.ui_attached
            && let Some(catalog) = &self.catalog
        {
            let _ = catalog.send(CatalogRequest::Cover(
                track,
                CoverTarget::Player,
                0,
                self.cover_generation,
            ));
        }
        (self.emit)(Event::Player(self.player.clone()));
        self.emit_queue();
        Ok(())
    }
    fn engine_event(&mut self, event: eagle_engine::Event) {
        let checkpoint = matches!(
            &event,
            eagle_engine::Event::TrackChanged(_)
                | eagle_engine::Event::StateChanged(State::Paused | State::Idle)
        );
        match event {
            eagle_engine::Event::TrackChanged(index) => {
                self.history_marker = Some((
                    index,
                    self.last_position
                        .as_ref()
                        .filter(|anchor| anchor.track == index)
                        .map(|anchor| anchor.position),
                ));
                let Some(track) = self.engine_queue.get(index as usize) else {
                    return;
                };
                let Some(current) = self.queue.iter().position(|item| Arc::ptr_eq(item, track))
                else {
                    return;
                };
                self.current = current;
                self.last_position = None;
                self.restored_position = Duration::ZERO;
                self.emit_queue();
                if let Some(track) = self.queue.get(self.current).cloned() {
                    let bitrate = self
                        .audio_bitrates
                        .get(&self.occurrence_id(&track))
                        .copied()
                        .flatten();
                    let state = Arc::make_mut(&mut self.player);
                    state.liked = self.queue_likes.contains(&track.id);
                    state.track = Some(track.clone());
                    state.bitrate_kbps = bitrate;
                    if self.ui_attached
                        && let Some(catalog) = &self.catalog
                    {
                        let _ = catalog.send(CatalogRequest::Cover(
                            track,
                            CoverTarget::Player,
                            0,
                            self.cover_generation,
                        ));
                    }
                    (self.emit)(Event::Player(self.player.clone()));
                }
            }
            eagle_engine::Event::StateChanged(status) => {
                if let Some(anchor) = &mut self.last_position {
                    anchor.position = position_now(anchor, Instant::now());
                    anchor.audible_at = Instant::now();
                    anchor.state = status;
                }
                Arc::make_mut(&mut self.player).status = status;
                (self.emit)(Event::Player(self.player.clone()));
            }
            eagle_engine::Event::Position(anchor) => {
                let Some(track) = self.engine_queue.get(anchor.track as usize).cloned() else {
                    return;
                };
                let occurrence = self.occurrence_id(&track);
                if occurrence == 0 || !self.queue.iter().any(|item| Arc::ptr_eq(item, &track)) {
                    return;
                }
                if let Some(current) = self.queue.iter().position(|item| Arc::ptr_eq(item, &track))
                    && current != self.current
                {
                    self.current = current;
                    let state = Arc::make_mut(&mut self.player);
                    state.track = Some(track.clone());
                    state.liked = self.queue_likes.contains(&track.id);
                    state.bitrate_kbps = self.audio_bitrates.get(&occurrence).copied().flatten();
                    state.source_provider = self.audio_sources.get(&occurrence).cloned();
                    self.emit_queue();
                    (self.emit)(Event::Player(self.player.clone()));
                }
                let repeated = self.history_marker.take().is_some_and(|(index, previous)| {
                    index == anchor.track
                        && previous.is_some_and(|previous| anchor.position < previous)
                }) && self.history_occurrence == Some(occurrence);
                if self.history_occurrence != Some(occurrence) || repeated {
                    self.history_occurrence = Some(occurrence);
                    self.history_recorded = false;
                }
                if !self.history_recorded
                    && anchor.state == State::Playing
                    && anchor.position >= Duration::from_secs(1)
                {
                    let account = self.account_of(&track.id);
                    // Radio intents belong to the account; without one the play is only in history.
                    let radio = (!account.is_empty())
                        .then(|| {
                            self.queue_radio
                                .get(&occurrence)
                                .map(|context| (occurrence, context))
                        })
                        .flatten();
                    match self
                        .store
                        .record_play_with_radio(&self.profile, account, &track, radio)
                    {
                        Ok(()) => self.history_recorded = true,
                        Err(error) => {
                            self.history_recorded = true;
                            (self.emit)(Event::Error(error.to_string()));
                        }
                    }
                }
                self.last_position = Some(anchor.clone());
                (self.emit)(Event::Position(anchor));
                if self.last_checkpoint.elapsed() >= Duration::from_secs(10)
                    && let Err(error) = self.checkpoint()
                {
                    (self.emit)(Event::Error(error.to_string()));
                }
            }
            eagle_engine::Event::Error { message, .. } => (self.emit)(Event::Error(message)),
            _ => {}
        }
        if checkpoint && let Err(error) = self.checkpoint() {
            (self.emit)(Event::Error(error.to_string()));
        }
    }
}

/// Likes without metadata that one sync pass asks the provider for.
const UNRESOLVED_BATCH: usize = 50;

#[derive(Default)]
struct SyncState {
    step: u8,
    /// Provider whose liked tracks are being read.
    provider: Option<Arc<dyn Provider>>,
    /// Providers whose likes are already handled in this run.
    done: Vec<String>,
    cursor: Option<String>,
    ids: Vec<String>,
}

enum CatalogRequest {
    RadioStart(u64, String),
    RadioMore(u64, String, RadioSession, Vec<RadioQueueItem>),
    Command(u64, Command),
    EditPlaylist(u64, PlaylistSnapshot, PlaylistEdit),
    RefreshPlaylist(u64, TrackId),
    Cover(Arc<Track>, CoverTarget, u64, u64),
    ClearCovers,
    /// Sends the due likes; `online` also makes the deferred ones due (the network came back).
    FlushLikes {
        online: bool,
    },
}
enum CatalogReply {
    RadioResult(String, bool, std::result::Result<RadioSession, RadioError>),
    Collections(Vec<Collection>),
    PlaylistChanged(Vec<Collection>, Option<Playlist>, Option<TrackId>),
    PlaylistFailed(String),
    PlaylistEdited(
        Option<PlaylistSnapshot>,
        Option<Vec<LibraryItem>>,
        Option<String>,
    ),
    Synced,
    /// Like `Synced`, and the provider's missing likes wait for the user's consent.
    SyncedOffer(String, usize),
    AlbumsSynced,
    ArtistsSynced,
    Cards(Vec<Card>),
    Page {
        items: Vec<LibraryItem>,
        title: String,
        header: Header,
        cards: Vec<Card>,
        cover: Option<crate::covers::Job>,
        playlist_snapshot: Option<PlaylistSnapshot>,
    },
    LikeFailed(TrackId, bool, String),
    CollectionLiked(TrackId, bool),
    Unchanged,
    Cover(CoverTarget, TrackId, Arc<eagle_palette::Cover>, u64),
    Tracks(Vec<LibraryItem>, String),
    Liked(TrackId, bool),
    Account(bool),
    /// Sign-in state changed without touching the account.
    Providers,
    /// More due likes are waiting: the worker queues another `FlushLikes` behind new requests.
    LikesPending,
}
struct Catalog {
    covers: RefCell<VecDeque<(TrackId, Arc<eagle_palette::Cover>)>>,
    /// Last opened album/artist, so a like from its header can be listed without a refetch.
    last_album: RefCell<Option<Album>>,
    last_artist: RefCell<Option<Artist>>,
    root: PathBuf,
    net: Arc<eagle_net::Client>,
    store: Arc<Store>,
    providers: Arc<Providers>,
    /// Set when a like flush stopped at a full batch with due likes left.
    likes_more: std::cell::Cell<bool>,
    clock: Clock,
}
impl Catalog {
    /// The provider that owns the track or collection `id`.
    fn provider_for(&self, id: &TrackId) -> Result<&Arc<dyn Provider>> {
        self.providers
            .for_track(id)
            .ok_or_else(|| "провайдер недоступен".into())
    }
    /// The first signed-in provider with the capability `f`.
    fn provider_with(&self, f: impl Fn(&Capabilities) -> bool) -> Result<&Arc<dyn Provider>> {
        self.providers
            .first_with(f)
            .ok_or_else(|| "войдите в аккаунт".into())
    }
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
                    CatalogRequest::ClearCovers => self.covers.borrow_mut().clear(),
                    CatalogRequest::Command(_, Command::SyncCollections | Command::SyncLibrary) => {
                        if sync.is_none() {
                            sync = Some(SyncState::default());
                        }
                    }
                    request => {
                        if matches!(&request, CatalogRequest::Command(_, Command::Login { .. })) {
                            sync = None;
                        } else if sync.is_some()
                            && matches!(&request, CatalogRequest::Command(_, Command::Like(_)))
                        {
                            sync = Some(SyncState::default());
                        }
                        let generation = match &request {
                            CatalogRequest::Command(generation, _) => *generation,
                            CatalogRequest::RadioStart(generation, _)
                            | CatalogRequest::RadioMore(generation, _, _, _) => *generation,
                            CatalogRequest::EditPlaylist(generation, _, _) => *generation,
                            CatalogRequest::RefreshPlaylist(generation, _) => *generation,
                            CatalogRequest::Cover(_, _, generation, _) => *generation,
                            CatalogRequest::FlushLikes { .. } => 0,
                            CatalogRequest::ClearCovers => unreachable!(),
                        };
                        let mutation = matches!(
                            &request,
                            CatalogRequest::Command(
                                _,
                                Command::CreatePlaylist(_) | Command::DeletePlaylist(_)
                            )
                        );
                        let send_partial = |partial: CatalogReply| {
                            let _ =
                                replies.send(Message::Catalog(generation, Box::new(Ok(partial))));
                        };
                        let reply =
                            match &request {
                                CatalogRequest::Command(_, Command::Search(query)) => {
                                    self.search_merged(query, &send_partial)
                                }
                                CatalogRequest::Command(_, Command::SearchArtists(query)) => self
                                    .search_cards_merged(CardKind::Artists, query, &send_partial),
                                CatalogRequest::Command(_, Command::SearchAlbums(query)) => {
                                    self.search_cards_merged(CardKind::Albums, query, &send_partial)
                                }
                                CatalogRequest::Command(_, Command::SearchPlaylists(query)) => self
                                    .search_cards_merged(CardKind::Playlists, query, &send_partial),
                                _ => self.request(request),
                            }
                            .map_err(|e| e.to_string());
                        let reply = if mutation {
                            Ok(reply.unwrap_or_else(CatalogReply::PlaylistFailed))
                        } else {
                            reply
                        };
                        if replies
                            .send(Message::Catalog(generation, Box::new(reply)))
                            .is_err()
                            || !self.report_likes_pending(&replies)
                        {
                            break;
                        }
                    }
                }
            } else if let Some(state) = &mut sync {
                match self.sync_step(state) {
                    Ok((reply, done)) => {
                        if let Some(reply) = reply
                            && replies
                                .send(Message::Catalog(0, Box::new(Ok(reply))))
                                .is_err()
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
                if !self.report_likes_pending(&replies) {
                    break;
                }
            }
        }
    }

    /// Tells the worker that another like batch is due. False when the worker has gone.
    fn report_likes_pending(&self, replies: &mpsc::Sender<Message>) -> bool {
        !self.likes_more.replace(false)
            || replies
                .send(Message::Catalog(
                    0,
                    Box::new(Ok(CatalogReply::LikesPending)),
                ))
                .is_ok()
    }

    /// Fetches the metadata of the provider's likes that are stored without a track. Each id is
    /// asked at most once per call. A missing or unavailable track keeps its like for a later
    /// call; a network, rate or sign-in failure stops the pass.
    fn resolve_unresolved_likes(&self, provider: &dyn Provider) {
        let provider_id = provider.info().id;
        let pending = match self.store.unresolved_likes(provider_id, UNRESOLVED_BATCH) {
            Ok(pending) => pending,
            Err(error) => {
                log::warn!(provider = provider_id, error = error.to_string().as_str(); "unresolved likes read failed");
                return;
            }
        };
        for id in pending {
            match provider.track(&id) {
                Ok(track) => {
                    if let Err(error) = self.store.put_tracks(&[track]) {
                        log::warn!(provider = provider_id, error = error.to_string().as_str(); "unresolved like store failed");
                        return;
                    }
                }
                Err(ProviderError::NotFound | ProviderError::Unavailable) => {}
                Err(error) => {
                    log::warn!(provider = provider_id, error = error.to_string().as_str(); "unresolved likes lookup stopped");
                    return;
                }
            }
        }
    }
    /// Next signed-in provider with likes that this sync run has not handled yet.
    fn next_likes_provider(&self, state: &SyncState) -> Option<Arc<dyn Provider>> {
        self.providers
            .with_capability(|c| c.likes)
            .find(|p| !state.done.iter().any(|d| d == p.info().id))
            .cloned()
    }
    fn sync_step(&self, state: &mut SyncState) -> Result<(Option<CatalogReply>, bool)> {
        match state.step {
            0 => {
                let synced = self
                    .providers
                    .first_with(|c| c.playlists_read)
                    .map(|p| crate::music::sync_playlists(&self.store, p.as_ref()));
                let reply = match synced {
                    Some(Ok(())) => Some(self.command(Command::Collections)?),
                    Some(Err(error)) => {
                        log::warn!(error = error.to_string().as_str(); "playlist sync failed");
                        None
                    }
                    None => None,
                };
                state.step = 1;
                Ok((reply, false))
            }
            1 => {
                let provider = match &state.provider {
                    Some(provider) => provider.clone(),
                    None => {
                        if state.done.is_empty() {
                            // Send what is waiting before reading the provider's list.
                            self.flush_likes();
                        }
                        let Some(next) = self.next_likes_provider(state) else {
                            state.step = 3;
                            return Ok((None, false));
                        };
                        state.provider = Some(next.clone());
                        state.cursor = None;
                        state.ids.clear();
                        next
                    }
                };
                let provider_id = provider.info().id;
                let stored = provider
                    .likes(state.cursor.as_deref())
                    .map_err(|error| error.to_string())
                    .and_then(|page| {
                        self.store
                            .put_tracks(&page.items)
                            .map(|()| page)
                            .map_err(|error| error.to_string())
                    });
                match stored {
                    Ok(page) => {
                        state
                            .ids
                            .extend(page.items.iter().map(|track| track.id.to_string()));
                        state.cursor = page.next;
                        if state.cursor.is_none() {
                            state.step = 2;
                        }
                    }
                    Err(error) => {
                        log::warn!(provider = provider_id, error = error.as_str(); "liked tracks sync failed");
                        state.done.push(provider_id.to_owned());
                        state.provider = None;
                        state.ids.clear();
                        if self.next_likes_provider(state).is_none() {
                            state.step = 3;
                        }
                    }
                }
                Ok((None, false))
            }
            2 => {
                let Some(provider) = state.provider.take() else {
                    state.step = 1;
                    return Ok((None, false));
                };
                let provider_id = provider.info().id;
                state.done.push(provider_id.to_owned());
                state.step = if self.next_likes_provider(state).is_some() {
                    1
                } else {
                    3
                };
                let ids: Vec<TrackId> = std::mem::take(&mut state.ids)
                    .iter()
                    .filter_map(|id| id.parse().ok())
                    .collect();
                match self.store.sync_provider_likes(provider_id, &ids) {
                    Ok(_) => {
                        self.resolve_unresolved_likes(provider.as_ref());
                        Ok((Some(self.backfill_after_sync(provider_id)), false))
                    }
                    Err(error) => {
                        log::warn!(provider = provider_id, error = error.to_string().as_str(); "liked tracks sync failed");
                        Ok((None, false))
                    }
                }
            }
            3 => {
                let reply = match self
                    .providers
                    .first_with(|c| c.liked_albums)
                    .map(|p| p.liked_albums())
                {
                    Some(Ok(albums)) => {
                        self.put_liked_albums(&albums)?;
                        Some(CatalogReply::AlbumsSynced)
                    }
                    Some(Err(error)) => {
                        log::warn!(error = error.to_string().as_str(); "liked albums sync failed");
                        None
                    }
                    None => None,
                };
                state.step = 4;
                Ok((reply, false))
            }
            _ => {
                let reply = match self
                    .providers
                    .first_with(|c| c.liked_artists)
                    .map(|p| p.liked_artists())
                {
                    Some(Ok(artists)) => {
                        self.put_liked_artists(&artists)?;
                        Some(CatalogReply::ArtistsSynced)
                    }
                    Some(Err(error)) => {
                        log::warn!(error = error.to_string().as_str(); "liked artists sync failed");
                        None
                    }
                    None => None,
                };
                Ok((reply, true))
            }
        }
    }

    fn request(&self, request: CatalogRequest) -> Result<CatalogReply> {
        match request {
            CatalogRequest::RadioStart(_, account) => {
                let result = match self.providers.first_with(|c| c.radio) {
                    Some(provider) => match provider.account() {
                        Ok(current) if current.id == account => provider.radio_start(),
                        _ => Err(eagle_core::ProviderError::Auth.into()),
                    },
                    None => Err(eagle_core::ProviderError::Auth.into()),
                };
                Ok(CatalogReply::RadioResult(account, true, result))
            }
            CatalogRequest::RadioMore(_, account, mut session, queue) => {
                // The session belongs to the provider of its tracks, whatever the priority is now.
                let owner = queue
                    .first()
                    .and_then(|item| self.providers.for_track(&item.track))
                    .filter(|p| {
                        self.providers.is_signed_in(p.as_ref())
                            && self.providers.effective(p.as_ref()).radio
                    });
                let result = match owner.or_else(|| self.providers.first_with(|c| c.radio)) {
                    Some(provider) => match provider.account() {
                        Ok(current) if current.id == account => {
                            provider.radio_next(&session, &queue).map(|batch| {
                                session.batch = batch;
                                session
                            })
                        }
                        _ => Err(eagle_core::ProviderError::Auth.into()),
                    },
                    None => Err(eagle_core::ProviderError::Auth.into()),
                };
                Ok(CatalogReply::RadioResult(account, false, result))
            }
            CatalogRequest::RefreshPlaylist(_, id) => {
                let message = "плейлист изменился; обновите список и повторите действие".to_owned();
                let snapshot = self
                    .provider_for(&id)
                    .ok()
                    .and_then(|provider| provider.playlist_snapshot(&id).ok())
                    .filter(|s| s.playlist.id == id);
                let rows = snapshot.as_ref().and_then(|s| self.cache_playlist(s).ok());
                Ok(CatalogReply::PlaylistEdited(snapshot, rows, Some(message)))
            }
            CatalogRequest::EditPlaylist(_, snapshot, edit) => {
                let id = snapshot.playlist.id.clone();
                let provider = self.provider_for(&id)?;
                let result = provider.edit_playlist(&snapshot, edit);
                let succeeded = result.is_ok();
                let (snapshot, error) = match result {
                    Ok(snapshot) if snapshot.playlist.id == id => (Some(snapshot), None),
                    Ok(_) => (
                        None,
                        Some("Провайдер вернул другой плейлист; обновите список".into()),
                    ),
                    Err(error @ (PlaylistEditError::Conflict | PlaylistEditError::Uncertain)) => (
                        provider
                            .playlist_snapshot(&id)
                            .ok()
                            .filter(|s| s.playlist.id == id),
                        Some(error.to_string()),
                    ),
                    Err(error) => (None, Some(error.to_string())),
                };
                let rows = if let Some(snapshot) = &snapshot {
                    match self.cache_playlist(snapshot) {
                        Ok(rows) => Some(rows),
                        Err(e) => {
                            let message = if succeeded {
                                "Изменение у провайдера выполнено, но локальный кэш не обновлён"
                            } else {
                                "Не удалось сохранить перечитанный плейлист локально"
                            };
                            let message = match &error {
                                Some(original) => format!(
                                    "{original}. {message}: {e}. Обновите плейлист вручную перед повтором"
                                ),
                                None => format!(
                                    "{message}: {e}. Обновите плейлист вручную перед повтором"
                                ),
                            };
                            return Ok(CatalogReply::PlaylistEdited(
                                snapshot.clone().into(),
                                None,
                                Some(message),
                            ));
                        }
                    }
                } else {
                    None
                };
                Ok(CatalogReply::PlaylistEdited(snapshot, rows, error))
            }
            CatalogRequest::ClearCovers => {
                self.covers.borrow_mut().clear();
                Ok(CatalogReply::Unchanged)
            }
            CatalogRequest::FlushLikes { online } => {
                if online && let Err(error) = self.store.wake_like_jobs() {
                    log::warn!(error = error.to_string().as_str(); "like wake failed");
                }
                self.flush_likes();
                Ok(CatalogReply::Unchanged)
            }
            CatalogRequest::Command(_, command) => self.command(command),
            CatalogRequest::Cover(track, target, _, cover_generation) => {
                {
                    let mut covers = self.covers.borrow_mut();
                    if let Some(index) = covers.iter().position(|(id, _)| id == &track.id) {
                        let entry = covers.remove(index).ok_or("обложка удалена из кэша")?;
                        let cover = entry.1.clone();
                        covers.push_front(entry);
                        return Ok(CatalogReply::Cover(
                            target,
                            track.id.clone(),
                            cover,
                            cover_generation,
                        ));
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
                Ok(CatalogReply::Cover(
                    target,
                    track.id.clone(),
                    cover,
                    cover_generation,
                ))
            }
        }
    }

    fn cache_playlist(&self, snapshot: &PlaylistSnapshot) -> Result<Vec<LibraryItem>> {
        let playlist = &snapshot.playlist;
        self.store.put_json(
            "playlists",
            &playlist.id.to_string(),
            serde_json::to_string(playlist)?,
        )?;
        match self.tracks(playlist.tracks.clone(), playlist.title.clone())? {
            CatalogReply::Tracks(rows, _) => Ok(rows),
            _ => Err("неожиданный ответ каталога".into()),
        }
    }
    /// The provider whose account owns this installation's rights, if any.
    fn account_id(&self) -> Option<&'static str> {
        self.providers.account_provider().map(|p| p.info().id)
    }
    fn put_liked_albums(&self, albums: &[Album]) -> Result<()> {
        if let Some(id) = self.account_id() {
            self.store
                .put_setting(&liked_albums_key(id), serde_json::to_string(albums)?)?;
        }
        Ok(())
    }
    fn put_liked_artists(&self, artists: &[Artist]) -> Result<()> {
        if let Some(id) = self.account_id() {
            self.store
                .put_setting(&liked_artists_key(id), serde_json::to_string(artists)?)?;
        }
        Ok(())
    }
    /// After a provider's likes were synced: sends the likes it lacks if the user agreed, offers
    /// to if the user has not been asked and there is something to send, and does nothing if the
    /// user declined.
    fn backfill_after_sync(&self, provider: &str) -> CatalogReply {
        let key = likes_backfill_key(provider);
        let answer = self.store.setting(&key).unwrap_or_else(|error| {
            log::warn!(provider = provider, error = error.to_string().as_str(); "backfill setting unreadable");
            Some("no".into())
        });
        match answer.as_deref() {
            Some("yes") => {
                if let Err(error) = self.store.queue_likes_backfill(provider) {
                    log::warn!(provider = provider, error = error.to_string().as_str(); "likes backfill failed");
                }
                self.flush_likes();
                CatalogReply::Synced
            }
            Some(_) => CatalogReply::Synced,
            None => match self.store.likes_backfill_candidates(provider) {
                Ok(found) if !found.is_empty() => {
                    CatalogReply::SyncedOffer(provider.to_owned(), found.len())
                }
                _ => CatalogReply::Synced,
            },
        }
    }
    /// Sends up to `LIKE_BATCH` due likes. A network or rate-limit failure defers the job with a
    /// growing backoff and stops this provider; a sign-in failure stops it without a mark. A full
    /// batch with due likes left sets `likes_more`.
    fn flush_likes(&self) {
        let now = (self.clock)();
        let mut sent = 0;
        for provider in self.providers.with_capability(|c| c.likes) {
            let provider_id = provider.info().id;
            let jobs = match self.store.like_jobs(provider_id, now) {
                Ok(jobs) => jobs,
                Err(error) => {
                    log::warn!(provider = provider_id, error = error.to_string().as_str(); "like queue unreadable");
                    continue;
                }
            };
            for job in jobs {
                if sent == LIKE_BATCH {
                    self.likes_more.set(true);
                    return;
                }
                let attempt = match job.track.parse::<TrackId>() {
                    Ok(track) => provider.set_liked(&track, job.liked),
                    Err(_) => Err(eagle_core::ProviderError::InvalidId),
                };
                let recorded = match attempt {
                    Ok(()) => self.store.finish_like_job(&job).map(|_| ()),
                    Err(eagle_core::ProviderError::Uncertain) => {
                        self.store.mark_like_job_uncertain(&job).map(|_| ())
                    }
                    Err(
                        eagle_core::ProviderError::Network | eagle_core::ProviderError::RateLimited,
                    ) => {
                        log::debug!(provider = provider_id, track_id = job.track.as_str(); "like deferred");
                        if let Err(error) = self.store.defer_like_job_backoff(&job, now) {
                            log::warn!(provider = provider_id, error = error.to_string().as_str(); "like deferral failed");
                        }
                        break;
                    }
                    Err(eagle_core::ProviderError::Auth) => break,
                    Err(error) => {
                        log::warn!(provider = provider_id, track_id = job.track.as_str(), error = error.to_string().as_str(); "like rejected");
                        self.store.finish_like_job(&job).map(|_| ())
                    }
                };
                if let Err(error) = recorded {
                    log::warn!(provider = provider_id, error = error.to_string().as_str(); "like queue update failed");
                }
                sent += 1;
            }
        }
    }
    fn switch_account(&self, uid: &str) -> Result<bool> {
        let Some(provider) = self.account_id() else {
            return Ok(false);
        };
        let uid = uid.to_owned();
        let account_setting = account_key(provider);
        let likes_prefix = format!("{provider}:%");
        let provider_id = provider.to_owned();
        let likes_synced = format!("likes_synced:{provider}");
        let albums_setting = liked_albums_key(provider);
        let artists_setting = liked_artists_key(provider);
        Ok(self.store.write(move |connection| {
            let tx = connection.transaction()?;
            let same: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM settings WHERE key=?1 AND value=?2)",
                [account_setting.as_str(), uid.as_str()],
                |row| row.get(0),
            )?;
            if !same {
                // Likes are the profile's; only what was learned from the old account goes.
                tx.execute(
                    "DELETE FROM provider_liked_seen WHERE provider=?1",
                    [provider_id.as_str()],
                )?;
                tx.execute(
                    "DELETE FROM like_outbox WHERE provider=?1",
                    [provider_id.as_str()],
                )?;
                tx.execute("DELETE FROM settings WHERE key=?1", [likes_synced.as_str()])?;
                tx.execute(
                    "DELETE FROM playlists WHERE id LIKE ?1",
                    [likes_prefix.as_str()],
                )?;
                tx.execute(
                    "DELETE FROM settings WHERE key IN (?1, ?2)",
                    [albums_setting.as_str(), artists_setting.as_str()],
                )?;
                tx.execute(
                    "INSERT OR REPLACE INTO settings VALUES (?1, ?2)",
                    [account_setting.as_str(), uid.as_str()],
                )?;
            }
            tx.commit()?;
            Ok(!same)
        })?)
    }
    fn library(&self) -> Result<CatalogReply> {
        // The library is the liked collection, newest like first, shown by the provider
        // with the highest priority.
        let priority: Vec<String> = self
            .providers
            .all()
            .iter()
            .map(|p| p.info().id.to_owned())
            .collect();
        let mut rows: Vec<LibraryItem> = self
            .store
            .liked_tracks(&priority)?
            .into_iter()
            .map(|track| LibraryItem {
                sources: Vec::new(),
                blocked_reason: None,
                track: Arc::new(track),
                liked: true,
            })
            .collect();
        self.apply_access(&mut rows)?;
        Ok(CatalogReply::Tracks(rows, String::new()))
    }
    fn apply_access(&self, rows: &mut [LibraryItem]) -> Result<()> {
        let account = stored_account(&self.store, &self.providers)?;
        for row in rows {
            let access = self.store.access(&account, &row.track.id.to_string())?;
            row.blocked_reason = if access.allows_playback() {
                None
            } else {
                Some(
                    access
                        .reason
                        .unwrap_or_else(|| "Доступ отозван провайдером".into()),
                )
            };
        }
        Ok(())
    }
    fn liked_albums(&self) -> Result<Vec<Album>> {
        let Some(id) = self.account_id() else {
            return Ok(Vec::new());
        };
        Ok(match self.store.setting(&liked_albums_key(id))? {
            Some(json) => serde_json::from_str(&json)?,
            None => Vec::new(),
        })
    }
    fn liked_artists(&self) -> Result<Vec<Artist>> {
        let Some(id) = self.account_id() else {
            return Ok(Vec::new());
        };
        Ok(match self.store.setting(&liked_artists_key(id))? {
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
            playlist_snapshot: None,
        })
    }
    /// Asks every signed-in provider that can search, one after another in priority order.
    /// After each provider except the last, `emit` gets the merged list so far; the same track
    /// from several providers is one row (the higher-priority copy) listing the other sources.
    fn search_merged(&self, query: &str, emit: &dyn Fn(CatalogReply)) -> Result<CatalogReply> {
        let providers: Vec<_> = self
            .providers
            .with_capability(|c| c.search_tracks)
            .collect();
        if providers.is_empty() {
            return Err("войдите в аккаунт".into());
        }
        let mut rows: Vec<(Track, Vec<TrackId>)> = Vec::new();
        let mut answered = false;
        let mut failure = None;
        for (at, provider) in providers.iter().enumerate() {
            match provider.search(query, 0) {
                Ok(page) => {
                    answered = true;
                    // Stored first, so the store groups the copies even when only one is shown.
                    self.store.put_tracks(&page.items)?;
                    for track in page.items {
                        let same = rows.iter_mut().find(|(shown, _)| {
                            eagle_core::matching::can_merge(shown, &track).is_some()
                        });
                        match same {
                            Some((_, sources)) => sources.push(track.id),
                            None => rows.push((track, Vec::new())),
                        }
                    }
                }
                Err(error) => {
                    log::warn!(provider = provider.info().id; "search failed: {error}");
                    failure = Some(error);
                    continue;
                }
            }
            if at + 1 < providers.len() {
                emit(self.merged_rows(&rows)?);
            }
        }
        match (answered, failure) {
            (false, Some(error)) => Err(error.into()),
            _ => self.merged_rows(&rows),
        }
    }
    /// Asks every signed-in provider that can search for `kind`, in priority order. Like
    /// `search_merged`, `emit` gets the list so far after each provider except the last. An
    /// artist that the store groups with an already shown one is folded into that card; albums
    /// and playlists are never merged. Fails only when no provider answered.
    fn search_cards_merged(
        &self,
        kind: CardKind,
        query: &str,
        emit: &dyn Fn(CatalogReply),
    ) -> Result<CatalogReply> {
        let capability: fn(&Capabilities) -> bool = match kind {
            CardKind::Artists => |c| c.search_artists,
            CardKind::Albums => |c| c.search_albums,
            CardKind::Playlists => |c| c.search_playlists,
        };
        let providers: Vec<_> = self.providers.with_capability(capability).collect();
        if providers.is_empty() {
            return Err("войдите в аккаунт".into());
        }
        // Each shown card with its artist group, if it has one.
        let mut rows: Vec<(Card, Option<u64>)> = Vec::new();
        let mut answered = false;
        let mut failure = None;
        for (at, provider) in providers.iter().enumerate() {
            match cards_of(provider.as_ref(), kind, query) {
                Ok(cards) => {
                    answered = true;
                    for card in cards {
                        let group = match kind {
                            CardKind::Artists => self.store.artist_group(&card.id)?,
                            CardKind::Albums | CardKind::Playlists => None,
                        };
                        let same = group
                            .and_then(|grp| rows.iter_mut().find(|(_, shown)| *shown == Some(grp)));
                        match same {
                            Some((shown, _)) => shown.sources.push(card.id),
                            None => rows.push((card, group)),
                        }
                    }
                }
                Err(error) => {
                    log::warn!(provider = provider.info().id; "card search failed: {error}");
                    failure = Some(error);
                    continue;
                }
            }
            if at + 1 < providers.len() {
                emit(cards_reply(&rows));
            }
        }
        match (answered, failure) {
            (false, Some(error)) => Err(error.into()),
            _ => Ok(cards_reply(&rows)),
        }
    }
    fn merged_rows(&self, rows: &[(Track, Vec<TrackId>)]) -> Result<CatalogReply> {
        let CatalogReply::Tracks(mut items, title) = self.tracks(
            rows.iter().map(|(track, _)| track.clone()).collect(),
            String::new(),
        )?
        else {
            return Err("не удалось подготовить результаты".into());
        };
        for (item, (_, sources)) in items.iter_mut().zip(rows) {
            item.sources = sources.clone();
        }
        Ok(CatalogReply::Tracks(items, title))
    }
    fn tracks(&self, tracks: Vec<Track>, title: String) -> Result<CatalogReply> {
        self.store.put_tracks(&tracks)?;
        let liked = self.store.liked_track_ids()?;
        let mut items = tracks
            .into_iter()
            .map(|track| LibraryItem {
                sources: Vec::new(),
                blocked_reason: None,
                liked: liked.contains(&track.id.to_string()),
                track: Arc::new(track),
            })
            .collect::<Vec<_>>();
        self.apply_access(&mut items)?;
        Ok(CatalogReply::Tracks(items, title))
    }
    fn refresh_access(&self, account: &str) -> Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs() as i64;
        for track in self.store.saved_tracks(account)? {
            let key = track.id.to_string();
            if !self.store.access(account, &key)?.access_due(now) {
                continue;
            }
            let checked = match self.provider_for(&track.id) {
                Ok(provider) => provider.check_access(&track.id),
                Err(_) => Err(eagle_core::ProviderError::Unavailable),
            };
            match checked {
                Ok(eagle_core::AccessCheck::Confirmed) => self.store.record_access(
                    account,
                    &key,
                    eagle_store::AccessState::Confirmed,
                    None,
                    now,
                )?,
                Ok(eagle_core::AccessCheck::Revoked) => self.store.record_access(
                    account,
                    &key,
                    eagle_store::AccessState::Revoked,
                    Some("Провайдер подтвердил потерю доступа".into()),
                    now,
                )?,
                Ok(eagle_core::AccessCheck::Unknown) | Err(_) => {
                    self.store.note_access_attempt(account, &key, now)?
                }
            }
        }
        Ok(())
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
            Command::Restore => {
                // Each provider restores on its own; one failing does not stop the others.
                let account_provider = self.account_id();
                let mut restored = false;
                let mut changed = false;
                for provider in self.providers.all() {
                    let Ok(account) = self.providers.restore(provider.as_ref()) else {
                        continue;
                    };
                    if Some(provider.info().id) == account_provider {
                        changed |= self.switch_account(&account.id)?;
                        self.refresh_access(&account.id)?;
                    }
                    self.resolve_unresolved_likes(provider.as_ref());
                    restored = true;
                }
                Ok(if restored {
                    CatalogReply::Account(changed)
                } else {
                    CatalogReply::Providers
                })
            }
            Command::SyncCollections => {
                crate::music::sync_playlists(
                    &self.store,
                    self.provider_with(|c| c.playlists_read)?.as_ref(),
                )?;
                self.command(Command::Collections)
            }
            Command::SyncLibrary => {
                crate::music::sync_likes(&self.store, self.provider_with(|c| c.likes)?.as_ref())?;
                // Album and artist likes are a bonus: their failure must not hide liked tracks.
                match self
                    .providers
                    .first_with(|c| c.liked_albums)
                    .map(|p| p.liked_albums())
                {
                    Some(Ok(albums)) => self.put_liked_albums(&albums)?,
                    Some(Err(error)) => {
                        log::warn!(error = error.to_string().as_str(); "liked albums sync failed")
                    }
                    None => {}
                }
                match self
                    .providers
                    .first_with(|c| c.liked_artists)
                    .map(|p| p.liked_artists())
                {
                    Some(Ok(artists)) => self.put_liked_artists(&artists)?,
                    Some(Err(error)) => {
                        log::warn!(error = error.to_string().as_str(); "liked artists sync failed")
                    }
                    None => {}
                }
                Ok(CatalogReply::Synced)
            }
            Command::Search(query) => self.search_merged(&query, &|_| {}),
            Command::SearchArtists(query) => {
                self.search_cards_merged(CardKind::Artists, &query, &|_| {})
            }
            Command::SearchAlbums(query) => {
                self.search_cards_merged(CardKind::Albums, &query, &|_| {})
            }
            Command::SearchPlaylists(query) => {
                self.search_cards_merged(CardKind::Playlists, &query, &|_| {})
            }
            Command::CreatePlaylist(title) => {
                let playlist = self
                    .provider_with(|c| c.playlists_edit)?
                    .create_playlist(&title)?;
                (|| -> Result<CatalogReply> {
                    self.store.put_json("playlists", &playlist.id.to_string(), serde_json::to_string(&playlist)?)?;
                    let CatalogReply::Collections(collections) = self.command(Command::Collections)? else { return Err("не удалось прочитать плейлисты".into()) };
                    Ok(CatalogReply::PlaylistChanged(collections, Some(playlist), None))
                })().map_err(|e| format!("Плейлист создан в Яндекс Музыке, но локальный список не обновлён. Обновите список; не создавайте его повторно. {e}").into())
            }
            Command::DeletePlaylist(id) => {
                self.provider_for(&id)?.delete_playlist(&id)?;
                (|| -> Result<CatalogReply> {
                    let key = id.to_string();
                    self.store.write(move |c| { c.execute("DELETE FROM playlists WHERE id=?1", [key])?; Ok(()) })?;
                    let CatalogReply::Collections(collections) = self.command(Command::Collections)? else { return Err("не удалось прочитать плейлисты".into()) };
                    Ok(CatalogReply::PlaylistChanged(collections, None, Some(id)))
                })().map_err(|e| format!("Плейлист удалён в Яндекс Музыке, но локальный список не обновлён. Обновите список. {e}").into())
            }
            Command::Album(id) => {
                let mut album = self.provider_for(&id)?.album(&id)?;
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
                let page = self.provider_for(&id)?.artist(&id)?;
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
                if let Err(error) = self.provider_for(&id)?.set_album_liked(&id, liked) {
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
                self.put_liked_albums(&albums)?;
                Ok(CatalogReply::CollectionLiked(id, liked))
            }
            Command::LikeArtist(id) => {
                let mut artists = self.liked_artists()?;
                let liked = !artists.iter().any(|a| a.id == id);
                if let Err(error) = self.provider_for(&id)?.set_artist_liked(&id, liked) {
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
                self.put_liked_artists(&artists)?;
                Ok(CatalogReply::CollectionLiked(id, liked))
            }
            Command::Playlist(id) => {
                let cached = self.store.read()?.query_row(
                    "SELECT EXISTS(SELECT 1 FROM playlists WHERE id=?1)",
                    [id.to_string()],
                    |r| r.get::<_, bool>(0),
                )?;
                let provider = self.provider_for(&id)?;
                let own = provider.account().ok().is_some_and(|account| {
                    id.local()
                        .split_once('_')
                        .is_some_and(|(owner, _)| owner == account.id)
                });
                let deletable = cached && own;
                let snapshot = if own {
                    provider
                        .playlist_snapshot(&id)
                        .ok()
                        .filter(|s| s.playlist.id == id)
                } else {
                    None
                };
                let p = match &snapshot {
                    Some(snapshot) => snapshot.playlist.clone(),
                    None => provider.playlist(&id)?,
                };
                let cover = header_cover(&id, "playlist", p.cover.clone());
                let mut page = self.page(
                    p.tracks,
                    p.title,
                    Header {
                        id: id.to_string(),
                        deletable,
                        editable: snapshot.is_some(),
                        playlist_revision: snapshot.as_ref().map(|s| s.revision),
                        ..Header::default()
                    },
                    Vec::new(),
                    cover,
                )?;
                if let CatalogReply::Page {
                    playlist_snapshot, ..
                } = &mut page
                {
                    *playlist_snapshot = snapshot;
                }
                Ok(page)
            }
            Command::Login { provider, token } => {
                let provider = self
                    .providers
                    .get(&provider)
                    .ok_or("провайдер недоступен")?
                    .clone();
                if !matches!(provider.info().auth, AuthMethod::Token { .. }) {
                    return Err("этот провайдер не требует входа токеном".into());
                }
                let account = self.providers.sign_in(provider.as_ref(), token)?;
                self.resolve_unresolved_likes(provider.as_ref());
                Ok(if self.account_id() == Some(provider.info().id) {
                    CatalogReply::Account(self.switch_account(&account.id)?)
                } else {
                    CatalogReply::Providers
                })
            }
            Command::LikesBackfill { provider, accept } => {
                if self.providers.get(&provider).is_none() {
                    return Err("провайдер недоступен".into());
                }
                self.store.put_setting(
                    &likes_backfill_key(&provider),
                    if accept { "yes" } else { "no" }.into(),
                )?;
                if accept {
                    self.store.queue_likes_backfill(&provider)?;
                    self.flush_likes();
                }
                Ok(CatalogReply::Unchanged)
            }
            Command::Like(id) => {
                let liked = !self.store.is_liked(&id)?;
                let targets: Vec<String> = self
                    .providers
                    .with_capability(|c| c.likes)
                    .map(|p| p.info().id.to_owned())
                    .collect();
                // Local first: the like is the profile's, providers get it from the outbox.
                if let Err(error) = self.store.like(&id, liked, &targets) {
                    // The UI already flipped the heart; tell it the real state.
                    return Ok(CatalogReply::LikeFailed(id, !liked, error.to_string()));
                }
                self.flush_likes();
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
            bitrate_kbps: self.bitrate_kbps,
            source_provider: self.source_provider.clone(),
            status: self.status,
            volume: self.volume,
            liked: self.liked,
            shuffle: self.shuffle,
            repeat: self.repeat,
        }
    }
}
/// Likes `id` in a test store, storing a minimal track first when it is not there yet.
#[cfg(test)]
fn seed_like(store: &Store, id: &str) {
    seed_track(store, id);
    store.like(&id.parse().unwrap(), true, &[]).unwrap();
}
/// Like `seed_like`, and the provider has listed it in an earlier sync, so a later sync that no
/// longer lists it removes it.
#[cfg(test)]
fn seed_synced_like(store: &Store, id: &str) {
    seed_like(store, id);
    let id: TrackId = id.parse().unwrap();
    let provider = id.provider().to_owned();
    store.sync_provider_likes(&provider, &[id]).unwrap();
}
#[cfg(test)]
fn seed_track(store: &Store, id: &str) {
    let id: TrackId = id.parse().unwrap();
    if store.track(&id).unwrap().is_none() {
        store
            .put_track(&Track {
                id: id.clone(),
                title: id.to_string(),
                artists: vec![],
                album: None,
                album_title: None,
                duration_ms: 1000,
                cover: None,
                availability: Availability::Available,
                isrc: None,
            })
            .unwrap();
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
    #[test]
    fn audible_position_uses_engine_occurrence_before_or_without_track_changed() {
        let root = std::env::temp_dir().join(format!("eg-audible-radio-{}", std::process::id()));
        let (tx, _) = mpsc::channel();
        let mut worker = Worker::new(root.clone(), Arc::new(|_| {}), tx).unwrap();
        worker.catalog.take();
        worker.catalog_worker.take().unwrap().join().unwrap();
        worker.account = "a".into();
        worker.queue = vec![
            row(1, false).track,
            row(1, false).track,
            row(2, false).track,
        ];
        worker.queue_original = worker.queue.clone();
        worker.assign_occurrences().unwrap();
        worker.engine_queue = worker.queue.clone();
        for (index, track) in worker.queue.iter().enumerate() {
            worker.queue_radio.insert(
                worker.occurrence_id(track),
                eagle_store::RadioContext {
                    account: "a".into(),
                    session_id: "s".into(),
                    batch_id: Some(format!("b{index}")),
                },
            );
        }
        Arc::make_mut(&mut worker.player).track = Some(worker.queue[0].clone());
        let position = |track| {
            eagle_engine::Event::Position(PositionAnchor {
                track,
                position: Duration::from_secs(1),
                audible_at: Instant::now(),
                state: State::Playing,
            })
        };
        worker.engine_event(position(1));
        let first: String = worker
            .store
            .read()
            .unwrap()
            .query_row("SELECT batch_id FROM radio_play_intents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(first, "b1");
        worker.engine_event(eagle_engine::Event::TrackChanged(1));
        worker.engine_event(position(1));
        worker.engine_event(position(2));
        let batches: Vec<String> = {
            let db = worker.store.read().unwrap();
            let mut query = db
                .prepare("SELECT batch_id FROM radio_play_intents ORDER BY rowid")
                .unwrap();
            query
                .query_map([], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(batches, ["b1", "b2"]);
        drop(worker);
        std::fs::remove_dir_all(root).unwrap();
    }
    fn play_audibly(worker: &mut Worker, track: Arc<Track>) {
        worker.queue = vec![track.clone()];
        worker.queue_original = worker.queue.clone();
        worker.engine_queue = worker.queue.clone();
        worker.assign_occurrences().unwrap();
        Arc::make_mut(&mut worker.player).track = Some(track);
        worker.engine_event(eagle_engine::Event::Position(PositionAnchor {
            track: 0,
            position: Duration::from_secs(2),
            audible_at: Instant::now(),
            state: State::Playing,
        }));
    }
    #[test]
    fn history_belongs_to_profile_and_play_reports_belong_to_account() {
        let root = std::env::temp_dir().join(format!("eg-session-profile-{}", std::process::id()));
        let (tx, _) = mpsc::channel();
        let mut worker = Worker::new(root.clone(), Arc::new(|_| {}), tx).unwrap();
        worker.catalog.take();
        worker.catalog_worker.take().unwrap().join().unwrap();
        // Without an account the play is recorded in history only, with no report queued.
        worker.account = String::new();
        play_audibly(&mut worker, row(1, false).track);
        assert_eq!(worker.store.history(&worker.profile).unwrap().len(), 1);
        assert!(worker.store.play_events("").unwrap().is_empty());
        worker.account = "a".into();
        play_audibly(&mut worker, row(2, false).track);
        assert_eq!(worker.store.history(&worker.profile).unwrap().len(), 2);
        assert_eq!(worker.store.play_events("a").unwrap().len(), 1);
        // History is shared by the profile across accounts; reports stay with their account.
        worker.account = "b".into();
        play_audibly(&mut worker, row(3, false).track);
        assert_eq!(worker.store.history(&worker.profile).unwrap().len(), 3);
        assert_eq!(worker.store.play_events("a").unwrap().len(), 1);
        assert_eq!(worker.store.play_events("b").unwrap().len(), 1);
        drop(worker);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn account_switch_keeps_profile_queue_and_position_and_drops_old_radio() {
        let root = std::env::temp_dir().join(format!("eg-account-queue-{}", std::process::id()));
        let (tx, _) = mpsc::channel();
        let mut worker = Worker::new(root.clone(), Arc::new(|_| {}), tx).unwrap();
        worker.catalog.take();
        worker.catalog_worker.take().unwrap().join().unwrap();
        worker.view = View::Search;
        worker.account = "a".into();
        worker
            .store
            .put_setting(&account_key("yandex"), "a".into())
            .unwrap();
        let track = row(1, false).track;
        worker.queue = vec![track.clone()];
        worker.queue_original = worker.queue.clone();
        worker.engine_queue = worker.queue.clone();
        worker.assign_occurrences().unwrap();
        let occurrence = worker.occurrence_id(&track);
        worker.queue_radio.insert(
            occurrence,
            eagle_store::RadioContext {
                account: "a".into(),
                session_id: "s".into(),
                batch_id: Some("b".into()),
            },
        );
        Arc::make_mut(&mut worker.player).track = Some(track.clone());
        worker.last_position = Some(PositionAnchor {
            track: 0,
            position: Duration::from_millis(500),
            audible_at: Instant::now(),
            state: State::Paused,
        });
        worker
            .store
            .put_setting(&account_key("yandex"), "b".into())
            .unwrap();
        worker.reset_account_view().unwrap();
        assert_eq!(worker.account, "b");
        assert_eq!(worker.queue.len(), 1);
        assert_eq!(worker.queue[0].id, track.id);
        assert!(worker.queue_radio.is_empty());
        assert_eq!(worker.restored_position, Duration::from_millis(500));
        let saved = worker.store.playback(&worker.profile).unwrap().unwrap();
        assert_eq!(saved.position_ms, 500);
        assert!(saved.entries.iter().all(|entry| entry.radio.is_none()));
        drop(worker);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn startup_with_another_accounts_radio_queue_keeps_tracks_instead_of_failing() {
        let root = std::env::temp_dir().join(format!("eg-foreign-radio-{}", std::process::id()));
        let (tx, _) = mpsc::channel();
        let mut worker = Worker::new(root.clone(), Arc::new(|_| {}), tx.clone()).unwrap();
        worker.catalog.take();
        worker.catalog_worker.take().unwrap().join().unwrap();
        worker.view = View::Search;
        worker.account = "a".into();
        let track = row(1, false).track;
        worker.queue = vec![track.clone()];
        worker.queue_original = worker.queue.clone();
        worker.engine_queue = worker.queue.clone();
        worker.assign_occurrences().unwrap();
        let occurrence = worker.occurrence_id(&track);
        worker.queue_radio.insert(
            occurrence,
            eagle_store::RadioContext {
                account: "a".into(),
                session_id: "s".into(),
                batch_id: None,
            },
        );
        Arc::make_mut(&mut worker.player).track = Some(track.clone());
        worker.checkpoint().unwrap();
        let store = worker.store.clone();
        drop(worker);
        // The user signed in with another account before the next launch.
        store
            .put_setting(&account_key("yandex"), "b".into())
            .unwrap();
        drop(store);
        let (tx, _) = mpsc::channel();
        let restarted = Worker::new(root.clone(), Arc::new(|_| {}), tx).unwrap();
        assert_eq!(restarted.queue.len(), 1);
        assert_eq!(restarted.queue[0].id, track.id);
        assert!(restarted.queue_radio.is_empty());
        drop(restarted);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn manual_radio_refill_appends_only_matching_session_and_keeps_occurrences() {
        let root = std::env::temp_dir().join(format!("eg-radio-refill-{}", std::process::id()));
        let (tx, _) = mpsc::channel();
        let mut worker = Worker::new(root.clone(), Arc::new(|_| {}), tx).unwrap();
        worker.catalog.take();
        worker.catalog_worker.take().unwrap().join().unwrap();
        worker.account = "a".into();
        let track = (*row(1, false).track).clone();
        let first = RadioBatch {
            id: Some("first".into()),
            tracks: vec![track.clone()],
        };
        worker.radio = Some((
            "a".into(),
            RadioSession {
                id: "s".into(),
                batch: first.clone(),
            },
            vec![first],
        ));
        worker.view = View::Radio;
        worker.publish_radio();
        worker.prepare_row(0).unwrap();
        worker.restored_position = Duration::from_secs(9);
        let old_id = worker.occurrence_id(&worker.queue[0]);
        let new_batch = RadioBatch {
            id: Some("next".into()),
            tracks: vec![track.clone(), track],
        };
        worker.radio_finished(
            "a".into(),
            false,
            Ok(RadioSession {
                id: "s".into(),
                batch: new_batch.clone(),
            }),
        );
        assert_eq!(worker.queue.len(), 3);
        assert_eq!(worker.current, 0);
        assert_eq!(worker.restored_position, Duration::from_secs(9));
        let ids: Vec<_> = worker
            .queue
            .iter()
            .map(|t| worker.occurrence_id(t))
            .collect();
        assert_eq!(ids[0], old_id);
        assert_ne!(ids[0], ids[1]);
        assert_ne!(ids[1], ids[2]);
        assert_eq!(
            worker.queue_radio[&ids[1]].batch_id.as_deref(),
            Some("next")
        );
        assert_eq!(
            worker.queue_radio[&ids[2]].batch_id.as_deref(),
            Some("next")
        );
        assert!(worker.store.history(&worker.profile).unwrap().is_empty());
        worker.queue_radio.clear();
        worker.radio_finished(
            "a".into(),
            false,
            Ok(RadioSession {
                id: "s".into(),
                batch: new_batch.clone(),
            }),
        );
        assert_eq!(worker.queue.len(), 3);
        worker.queue_radio.insert(
            old_id,
            eagle_store::RadioContext {
                account: "a".into(),
                session_id: "s".into(),
                batch_id: Some("first".into()),
            },
        );
        worker.next_queue_id = u64::MAX;
        worker.radio_finished(
            "a".into(),
            false,
            Ok(RadioSession {
                id: "s".into(),
                batch: new_batch,
            }),
        );
        assert_eq!(worker.queue.len(), 3);
        assert!(worker.radio_notice.is_some());
        assert!(worker.store.history(&worker.profile).unwrap().is_empty());
        drop(worker);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn radio_open_is_offline_and_manual_batches_keep_order_queue_and_duplicates() {
        let root = std::env::temp_dir().join(format!("eg-radio-manual-{}", std::process::id()));
        let (tx, _) = mpsc::channel();
        let mut worker = Worker::new(root.clone(), Arc::new(|_| {}), tx).unwrap();
        worker.catalog.take();
        worker.catalog_worker.take().unwrap().join().unwrap();
        let (catalog_tx, catalog_rx) = mpsc::channel();
        worker.catalog = Some(catalog_tx);
        worker.account = "a".into();
        worker.queue = vec![row(9, false).track];
        worker.command(Command::Open(View::Radio)).unwrap();
        assert!(catalog_rx.try_recv().is_err());
        assert_eq!(worker.queue[0].id.to_string(), "yandex:9");
        worker.command(Command::RadioStart).unwrap();
        assert!(matches!(
            catalog_rx.try_recv(),
            Ok(CatalogRequest::RadioStart(_, _))
        ));
        let track = (*row(1, false).track).clone();
        worker.radio_finished(
            "a".into(),
            true,
            Ok(RadioSession {
                id: "s".into(),
                batch: RadioBatch {
                    id: Some("b1".into()),
                    tracks: vec![track.clone(), track.clone()],
                },
            }),
        );
        assert_eq!(worker.rows.len(), 2);
        assert_eq!(worker.queue[0].id.to_string(), "yandex:9");
        worker.command(Command::RadioMore).unwrap();
        let Ok(CatalogRequest::RadioMore(_, _, _, queue)) = catalog_rx.try_recv() else {
            panic!("expected manual next")
        };
        assert_eq!(queue.len(), 2);
        assert_eq!(queue[0].track, queue[1].track);
        worker.radio_finished(
            "a".into(),
            false,
            Ok(RadioSession {
                id: "s".into(),
                batch: RadioBatch {
                    id: Some("b2".into()),
                    tracks: vec![track],
                },
            }),
        );
        assert_eq!(worker.rows.len(), 3);
        worker.command(Command::RadioMore).unwrap();
        let _ = catalog_rx.try_recv().unwrap();
        worker.radio_finished(
            "a".into(),
            false,
            Err(eagle_core::ProviderError::Network.into()),
        );
        assert_eq!(worker.rows.len(), 3);
        assert!(worker.snapshot().radio_notice.is_some());
        assert!(catalog_rx.try_recv().is_err());
        worker.command(Command::RadioMore).unwrap();
        let _ = catalog_rx.try_recv().unwrap();
        worker.radio_finished("a".into(), false, Err(RadioError::Uncertain));
        assert_eq!(worker.rows.len(), 3);
        assert!(!worker.snapshot().radio_ready);
        assert!(worker.command(Command::RadioMore).is_err());
        assert!(catalog_rx.try_recv().is_err());
        assert_eq!(worker.queue[0].id.to_string(), "yandex:9");
        drop(worker);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn radio_duplicate_occurrences_keep_batch_context_through_restart_and_audible_history() {
        let root = std::env::temp_dir().join(format!("eg-radio-occurrence-{}", std::process::id()));
        let (tx, _) = mpsc::channel();
        let mut worker = Worker::new(root.clone(), Arc::new(|_| {}), tx).unwrap();
        worker.catalog.take();
        worker.catalog_worker.take().unwrap().join().unwrap();
        worker.account = "a".into();
        let track = (*row(1, false).track).clone();
        let first = RadioBatch {
            id: Some("one".into()),
            tracks: vec![track.clone()],
        };
        let second = RadioBatch {
            id: Some("two".into()),
            tracks: vec![track],
        };
        worker.radio = Some((
            "a".into(),
            RadioSession {
                id: "session".into(),
                batch: second.clone(),
            },
            vec![first, second],
        ));
        worker.view = View::Radio;
        worker.publish_radio();
        worker.prepare_row(1).unwrap();
        let first_id = worker.occurrence_id(&worker.queue[0]);
        let second_id = worker.occurrence_id(&worker.queue[1]);
        assert_ne!(first_id, second_id);
        assert_eq!(
            worker.queue_radio[&first_id].batch_id.as_deref(),
            Some("one")
        );
        assert_eq!(
            worker.queue_radio[&second_id].batch_id.as_deref(),
            Some("two")
        );
        assert!(worker.store.play_events("a").unwrap().is_empty());
        worker.checkpoint().unwrap();
        let selected = worker.queue[1].clone();
        worker.engine_queue = worker.queue.clone();
        Arc::make_mut(&mut worker.player).track = Some(selected);
        worker.engine_event(eagle_engine::Event::Position(PositionAnchor {
            track: 1,
            position: Duration::from_secs(1),
            audible_at: Instant::now(),
            state: State::Playing,
        }));
        let result: (String, i64) = worker
            .store
            .read()
            .unwrap()
            .query_row(
                "SELECT batch_id,occurrence_id FROM radio_play_intents",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(result, ("two".into(), second_id as i64));
        worker.queue_radio.clear();
        worker.restore_playback().unwrap();
        assert_eq!(
            worker.queue_radio[&first_id].batch_id.as_deref(),
            Some("one")
        );
        assert_eq!(
            worker.queue_radio[&second_id].batch_id.as_deref(),
            Some("two")
        );
        drop(worker);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn hidden_playlist_edit_snapshot_keeps_busy_and_scoped_uncertain_result() {
        let root = std::env::temp_dir().join(format!("eg-hidden-edit-{}", std::process::id()));
        let (tx, _) = mpsc::channel();
        let mut worker = Worker::new(root.clone(), Arc::new(|_| {}), tx).unwrap();
        worker.catalog.take();
        worker.catalog_worker.take().unwrap().join().unwrap();
        let (catalog_tx, catalog_rx) = mpsc::channel();
        worker.catalog = Some(catalog_tx);
        let id: TrackId = "yandex:1_1".parse().unwrap();
        worker.view = View::Playlist;
        worker.playlist_id = Some(id.clone());
        worker.header.editable = true;
        worker.playlist_snapshot = Some(PlaylistSnapshot {
            playlist: Playlist {
                id: id.clone(),
                title: "test".into(),
                tracks: vec![],
                cover: None,
            },
            revision: 8,
        });
        worker.command(Command::UiAttached(false)).unwrap();
        assert!(matches!(
            catalog_rx.try_recv(),
            Ok(CatalogRequest::ClearCovers)
        ));
        worker
            .command(Command::EditPlaylist {
                id: id.clone(),
                revision: 8,
                edit: PlaylistEdit::Delete { from: 0, to: 1 },
            })
            .unwrap();
        assert!(matches!(
            catalog_rx.try_recv(),
            Ok(CatalogRequest::EditPlaylist(..))
        ));
        assert!(worker.snapshot().playlist_mutation_busy);
        worker.playlist_edit_finished(
            worker.view_generation,
            None,
            None,
            Some("результат изменения неизвестен".into()),
        );
        worker.command(Command::UiAttached(true)).unwrap();
        let snapshot = worker.snapshot();
        assert!(!snapshot.playlist_mutation_busy);
        assert_eq!(
            snapshot.playlist_mutation_notice.as_deref(),
            Some("результат изменения неизвестен")
        );
        assert!(catalog_rx.try_recv().is_err());
        worker.playlist_id = Some("yandex:1_2".parse().unwrap());
        assert!(worker.snapshot().playlist_mutation_notice.is_none());
        worker.playlist_id = Some(id.clone());
        assert!(worker.snapshot().playlist_mutation_notice.is_some());
        worker
            .command(Command::ClearPlaylistMutationNotice(id))
            .unwrap();
        assert!(worker.snapshot().playlist_mutation_notice.is_none());
        drop(worker);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn stale_playlist_popup_requests_refresh_without_mutating() {
        let root = std::env::temp_dir().join(format!("eg-stale-playlist-{}", std::process::id()));
        let (tx, _) = mpsc::channel();
        let mut worker = Worker::new(root.clone(), Arc::new(|_| {}), tx).unwrap();
        worker.catalog.take();
        worker.catalog_worker.take().unwrap().join().unwrap();
        let (catalog_tx, catalog_rx) = mpsc::channel();
        worker.catalog = Some(catalog_tx);
        let id: TrackId = "yandex:1_1".parse().unwrap();
        worker.view = View::Playlist;
        worker.playlist_id = Some(id.clone());
        worker.header.editable = true;
        worker.playlist_snapshot = Some(PlaylistSnapshot {
            playlist: Playlist {
                id: id.clone(),
                title: "test".into(),
                tracks: vec![
                    (*row(1, false).track).clone(),
                    (*row(1, false).track).clone(),
                ],
                cover: None,
            },
            revision: 8,
        });
        worker.rows = vec![row(1, false), row(1, false)];
        worker
            .command(Command::EditPlaylist {
                id,
                revision: 7,
                edit: PlaylistEdit::Delete { from: 1, to: 2 },
            })
            .unwrap();
        assert!(matches!(
            catalog_rx.try_recv(),
            Ok(CatalogRequest::RefreshPlaylist(_, _))
        ));
        assert!(catalog_rx.try_recv().is_err());
        assert_eq!(worker.rows.len(), 2);
        drop(worker);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn detached_snapshot_skips_covers_and_reattach_requests_them_again() {
        let root = std::env::temp_dir().join(format!("eg-session-cover-{}", std::process::id()));
        let (tx, _) = mpsc::channel();
        let mut worker = Worker::new(root.clone(), Arc::new(|_| {}), tx).unwrap();
        worker.catalog.take();
        worker.catalog_worker.take().unwrap().join().unwrap();
        let (catalog_tx, catalog_rx) = mpsc::channel();
        worker.catalog = Some(catalog_tx);
        let track = row(1, false).track;
        Arc::make_mut(&mut worker.player).track = Some(track.clone());
        worker.rows = vec![row(1, false)];
        worker.command(Command::UiAttached(false)).unwrap();
        assert!(matches!(
            catalog_rx.try_recv(),
            Ok(CatalogRequest::ClearCovers)
        ));
        worker.command(Command::Snapshot).unwrap();
        assert!(catalog_rx.try_recv().is_err());
        let old_generation = worker.cover_generation;
        worker.command(Command::UiAttached(true)).unwrap();
        assert_ne!(worker.cover_generation, old_generation);
        worker.command(Command::Snapshot).unwrap();
        assert!(matches!(
            catalog_rx.try_recv(),
            Ok(CatalogRequest::Cover(_, CoverTarget::Player, _, _))
        ));
        assert!(matches!(
            catalog_rx.try_recv(),
            Ok(CatalogRequest::Cover(_, CoverTarget::Collection, _, _))
        ));
        drop(worker);
        std::fs::remove_dir_all(root).unwrap();
    }
    fn row(id: u32, liked: bool) -> LibraryItem {
        LibraryItem {
            sources: Vec::new(),
            blocked_reason: None,
            track: Arc::new(Track {
                id: format!("yandex:{id}").parse().unwrap(),
                title: id.to_string(),
                artists: vec![],
                album: None,
                album_title: None,
                duration_ms: 1000,
                cover: None,
                availability: Availability::Available,
                isrc: None,
            }),
            liked,
        }
    }
    #[test]
    fn history_records_audible_play_once_and_separates_repeat_occurrences() {
        let root = std::env::temp_dir().join(format!("eg-session-history-{}", std::process::id()));
        let (tx, _) = mpsc::channel();
        let mut worker = Worker::new(root.clone(), Arc::new(|_| {}), tx).unwrap();
        worker.catalog.take();
        worker.catalog_worker.take().unwrap().join().unwrap();
        worker.account = "a".into();
        let track = row(1, false).track;
        worker.queue = vec![track.clone()];
        worker.queue_original = worker.queue.clone();
        worker.engine_queue = worker.queue.clone();
        worker.assign_occurrences().unwrap();
        Arc::make_mut(&mut worker.player).track = Some(track);
        let anchor = |seconds, state| {
            eagle_engine::Event::Position(PositionAnchor {
                track: 0,
                position: Duration::from_secs(seconds),
                audible_at: Instant::now(),
                state,
            })
        };
        worker.engine_event(anchor(0, State::Playing));
        worker.engine_event(anchor(2, State::Paused));
        assert!(worker.store.history(&worker.profile).unwrap().is_empty());
        worker.engine_event(anchor(1, State::Playing));
        worker.engine_event(anchor(2, State::Playing));
        assert_eq!(worker.store.history(&worker.profile).unwrap().len(), 1);
        worker.engine_event(eagle_engine::Event::TrackChanged(0));
        worker.engine_event(anchor(1, State::Playing));
        assert_eq!(worker.store.history(&worker.profile).unwrap().len(), 2);
        assert_eq!(worker.store.history(&worker.profile).unwrap().len(), 2);
        drop(worker);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn bitrate_is_scoped_to_engine_generation_and_selected_track() {
        let root = std::env::temp_dir().join(format!("eagle-bitrate-{}", std::process::id()));
        let (tx, _) = mpsc::channel();
        let mut worker = Worker::new(root.clone(), Arc::new(|_| {}), tx).unwrap();
        worker.catalog.take();
        worker.catalog_worker.take().unwrap().join().unwrap();
        let current = row(1, false).track;
        let next = Arc::new((*current).clone());
        worker.queue = vec![current.clone(), next.clone()];
        worker.queue_original = worker.queue.clone();
        worker.assign_occurrences().unwrap();
        let first = worker.occurrence_id(&current);
        let second = worker.occurrence_id(&next);
        assert_ne!(first, second);
        Arc::make_mut(&mut worker.player).track = Some(current.clone());
        worker.apply_bitrate(worker.engine_generation.wrapping_add(1), first, Some(320));
        assert_eq!(worker.player.bitrate_kbps, None);
        worker.apply_bitrate(worker.engine_generation, second, Some(128));
        assert_eq!(worker.player.bitrate_kbps, None);
        worker.apply_bitrate(worker.engine_generation, first, Some(320));
        assert_eq!(worker.snapshot().player.bitrate_kbps, Some(320));
        worker.apply_bitrate(worker.engine_generation, first, None);
        assert_eq!(worker.player.bitrate_kbps, None);
        worker.queue = vec![current, next];
        worker.select_paused(1);
        assert_eq!(worker.player.bitrate_kbps, None);
        drop(worker);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn revoked_access_updates_rows_and_stops_current_playback() {
        let root =
            std::env::temp_dir().join(format!("eagle-revoke-current-{}", std::process::id()));
        let (tx, _) = mpsc::channel();
        let mut worker = Worker::new(root.clone(), Arc::new(|_| {}), tx).unwrap();
        worker.account = "a".into();
        let item = row(1, false);
        worker.rows = vec![item.clone()];
        worker.queue = vec![item.track.clone()];
        worker.queue_original = worker.queue.clone();
        worker.assign_occurrences().unwrap();
        Arc::make_mut(&mut worker.player).track = Some(item.track.clone());
        Arc::make_mut(&mut worker.player).status = State::Playing;
        worker
            .store
            .record_access(
                "a",
                &item.track.id.to_string(),
                eagle_store::AccessState::Revoked,
                Some("Доступ отозван".into()),
                1,
            )
            .unwrap();
        worker.refresh_local_access().unwrap();
        assert_eq!(worker.player.status, State::Paused);
        assert_eq!(
            worker.rows[0].blocked_reason.as_deref(),
            Some("Доступ отозван")
        );
        worker
            .store
            .record_access(
                "a",
                &item.track.id.to_string(),
                eagle_store::AccessState::Confirmed,
                None,
                2,
            )
            .unwrap();
        worker.refresh_local_access().unwrap();
        assert!(worker.rows[0].blocked_reason.is_none());
        assert_eq!(worker.player.status, State::Paused);
        drop(worker);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn local_open_checks_current_account_rights_after_source_creation() {
        let root = std::env::temp_dir().join(format!("eagle-access-open-{}", std::process::id()));
        let (tx, _) = mpsc::channel();
        let mut worker = Worker::new(root.clone(), Arc::new(|_| {}), tx).unwrap();
        worker.account = "a".into();
        let track = row(1, false).track;
        let mut map = eagle_store::RangeMap::new(4);
        worker
            .cache
            .write_chunk(
                &worker.store,
                &track.id,
                Quality::High,
                &mut map,
                0,
                b"test",
            )
            .unwrap();
        worker
            .cache
            .pin(&worker.store, &track.id, Quality::High, true)
            .unwrap();
        worker
            .store
            .record_audio_owner("a", &track.id.to_string())
            .unwrap();
        let source = worker.source(&track).unwrap();
        let interrupt = || eagle_engine::Interrupt {
            generation: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            expected: 0,
            preload: false,
            seeking: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        assert!(source.open_reader(interrupt()).is_ok());
        worker
            .store
            .record_access(
                "a",
                &track.id.to_string(),
                eagle_store::AccessState::Revoked,
                None,
                1,
            )
            .unwrap();
        assert!(source.open_reader(interrupt()).is_err());
        assert!(worker.cache.path(&track.id, Quality::High).exists());
        worker
            .store
            .record_access(
                "a",
                &track.id.to_string(),
                eagle_store::AccessState::Confirmed,
                None,
                2,
            )
            .unwrap();
        assert!(source.open_reader(interrupt()).is_ok());
        drop(source);
        drop(worker);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn revoked_saved_rows_cannot_enter_play_all_or_selected_queue() {
        let mut blocked = row(1, false);
        blocked.blocked_reason = Some("Доступ отозван".into());
        let rows = vec![blocked, row(2, false)];
        assert!(initial_queue(&rows, 0).is_err());
        let (queue, current) = initial_queue(&rows, 1).unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(current, 0);
        assert_eq!(select_play_all(&rows, false, |_| 0), Some(1));
    }
    #[test]
    fn storage_move_restarts_writers_and_preserves_live_command_channel() {
        let root = std::env::temp_dir().join(format!("eagle-runtime-move-{}", std::process::id()));
        let (source, destination, bootstrap) = (
            root.join("source"),
            root.join("destination"),
            root.join("bootstrap"),
        );
        for path in [&source, &destination, &bootstrap] {
            std::fs::create_dir_all(path).unwrap();
        }
        let provider = crate::fixture::provider();
        let track = provider.likes(None).unwrap().items.remove(0);
        {
            let store = Store::open(&source).unwrap();
            store
                .put_setting(&account_key(provider.info().id), "fixture".into())
                .unwrap();
            store.put_track(&track).unwrap();
            let cache = Cache::open(&source.join("audio"), 1024, &store).unwrap();
            let mut map = eagle_store::RangeMap::new(4);
            cache
                .write_chunk(&store, &track.id, Quality::High, &mut map, 0, b"test")
                .unwrap();
            cache.pin(&store, &track.id, Quality::High, true).unwrap();
        }
        let (events, received) = mpsc::channel();
        let session = Session::start_with_providers(source.clone(), vec![provider], move |event| {
            let _ = events.send(event);
        })
        .unwrap();
        let sender = session.sender();
        sender.send(Command::EnqueueNext(track.id.clone())).unwrap();
        let lease =
            Arc::new(eagle_store::relocation::StorageLease::claim_empty(&destination).unwrap());
        sender
            .send(Command::RelocateStorage {
                destination: destination.clone(),
                bootstrap,
                lease: Some(lease.clone()),
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let event = received
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap();
            if let Event::Storage { folder, .. } = event
                && folder.canonicalize().unwrap() == destination.canonicalize().unwrap()
            {
                break;
            }
        }
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(destination.join("desktop.lock"))
            .unwrap();
        assert!(matches!(
            lock.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        sender.send(Command::Snapshot).unwrap();
        loop {
            if let Event::Snapshot(snapshot) =
                received.recv_timeout(Duration::from_secs(5)).unwrap()
            {
                assert_eq!(snapshot.queue.len(), 1);
                assert_eq!(snapshot.queue[0].1.id, track.id);
                break;
            }
        }
        drop(session);
        let store = Store::open(&destination).unwrap();
        let path: String = store
            .read()
            .unwrap()
            .query_row("SELECT path FROM cache LIMIT 1", [], |row| row.get(0))
            .unwrap();
        assert!(std::path::Path::new(&path).starts_with(destination.canonicalize().unwrap()));
        assert!(source.join("metadata.sqlite").exists());
        drop(store);
        drop(lease);
        drop(lock);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn successive_keyboard_moves_follow_occurrence_after_each_move() {
        let root = std::env::temp_dir().join(format!("eagle-shift-{}", std::process::id()));
        let (tx, _) = mpsc::channel();
        let mut worker = Worker::new(root.clone(), Arc::new(|_| {}), tx).unwrap();
        worker.catalog.take();
        worker.catalog_worker.take().unwrap().join().unwrap();
        worker.queue = vec![
            row(1, false).track,
            row(1, false).track,
            row(2, false).track,
        ];
        worker.queue_original = worker.queue.clone();
        worker.assign_occurrences().unwrap();
        let id = worker.occurrence_id(&worker.queue[0]);
        worker
            .command(Command::ShiftQueue { id, delta: 1 })
            .unwrap();
        worker
            .command(Command::ShiftQueue { id, delta: 1 })
            .unwrap();
        assert_eq!(worker.occurrence_id(&worker.queue[2]), id);
        drop(worker);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn durable_queue_restores_duplicates_paused_and_reorders_exact_occurrence() {
        let root = std::env::temp_dir().join(format!("eagle-durable-{}", std::process::id()));
        let (tx, _) = mpsc::channel();
        let mut worker = Worker::new(root.clone(), Arc::new(|_| {}), tx).unwrap();
        worker.catalog.take();
        worker.catalog_worker.take().unwrap().join().unwrap();
        let a = row(1, true).track;
        let duplicate = Arc::new((*a).clone());
        let b = row(2, false).track;
        worker.queue_original = vec![a.clone(), duplicate.clone(), b.clone()];
        worker.queue = vec![b.clone(), duplicate, a];
        worker.current = 1;
        Arc::make_mut(&mut worker.player).shuffle = true;
        Arc::make_mut(&mut worker.player).repeat = Repeat::All;
        worker.last_position = Some(PositionAnchor {
            track: 1,
            position: Duration::from_millis(750),
            audible_at: Instant::now(),
            state: State::Paused,
        });
        worker.checkpoint().unwrap();
        drop(worker);
        let (tx, _) = mpsc::channel();
        let mut worker = Worker::new(root.clone(), Arc::new(|_| {}), tx).unwrap();
        worker.catalog.take();
        worker.catalog_worker.take().unwrap().join().unwrap();
        assert!(worker.engine.is_none());
        assert_eq!(worker.player.status, State::Paused);
        assert_eq!(worker.restored_position, Duration::from_millis(750));
        let snapshot = worker.snapshot();
        assert_eq!(snapshot.current, Some(2));
        assert_eq!(
            snapshot.queue.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![3, 2, 1]
        );
        assert!(snapshot.player.shuffle);
        assert_eq!(snapshot.player.repeat, Repeat::All);
        worker.command(Command::PlayNext(3)).unwrap();
        assert_eq!(
            worker
                .snapshot()
                .queue
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>(),
            vec![2, 3, 1]
        );
        assert_eq!(worker.snapshot().current, Some(2));
        worker.command(Command::MoveQueue { id: 1, to: 0 }).unwrap();
        assert_eq!(worker.snapshot().current, Some(2));
        let id = worker.queue[0].id.clone();
        worker.store.put_track(worker.queue[0].as_ref()).unwrap();
        worker.command(Command::EnqueueNext(id)).unwrap();
        let snapshot = worker.snapshot();
        assert_eq!(snapshot.queue.len(), 4);
        assert_eq!(snapshot.current, Some(2));
        assert_eq!(snapshot.queue[2].0, 4);
        worker.command(Command::Next).unwrap();
        assert_eq!(worker.snapshot().current, Some(4));
        assert!(worker.engine.is_none());
        assert_eq!(worker.player.status, State::Paused);
        worker.command(Command::Previous).unwrap();
        assert_eq!(worker.snapshot().current, Some(2));
        let before = worker
            .store
            .playback(&worker.profile)
            .unwrap()
            .unwrap()
            .position_ms;
        let anchor = PositionAnchor {
            track: 0,
            position: Duration::from_millis(250),
            audible_at: Instant::now(),
            state: State::Paused,
        };
        worker.engine_queue = vec![worker.queue[worker.current].clone()];
        worker.engine_event(eagle_engine::Event::Position(anchor.clone()));
        assert_eq!(
            worker
                .store
                .playback(&worker.profile)
                .unwrap()
                .unwrap()
                .position_ms,
            before
        );
        worker.last_checkpoint = Instant::now() - Duration::from_secs(10);
        worker.engine_event(eagle_engine::Event::Position(anchor));
        assert_eq!(
            worker
                .store
                .playback(&worker.profile)
                .unwrap()
                .unwrap()
                .position_ms,
            250
        );
        drop(worker);
        std::fs::remove_dir_all(root).unwrap();
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
        worker.queue_original = worker.queue.clone();
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
    fn account_change_resets_player_queue_and_visible_likes() {
        let root = std::env::temp_dir().join(format!("eagle-account-view-{}", std::process::id()));
        let (events_tx, events_rx) = mpsc::channel();
        let (tx, _) = mpsc::channel();
        let mut worker = Worker::new(
            root.clone(),
            Arc::new(move |event| {
                let _ = events_tx.send(event);
            }),
            tx,
        )
        .unwrap();
        worker.view = View::Search;
        let item = row(1, true);
        worker.rows = vec![item.clone(), item.clone()];
        worker.queue_likes.insert(item.track.id.clone());
        Arc::make_mut(&mut worker.player).track = Some(item.track);
        Arc::make_mut(&mut worker.player).liked = true;
        worker.reset_account_view().unwrap();
        assert!(worker.queue_likes.is_empty());
        assert!(!worker.player.liked);
        assert!(worker.rows.iter().all(|row| !row.liked));
        assert!(worker.rows.is_empty());
        worker.publish(vec![row(1, false), row(1, false)]);
        let id = worker.rows[0].track.id.clone();
        worker.apply_liked(id.clone(), true);
        assert!(worker.player.track.is_none());
        assert!(worker.rows.iter().all(|row| row.liked));
        worker.apply_liked(id, false);
        assert!(!worker.player.liked);
        assert!(worker.rows.iter().all(|row| !row.liked));
        assert!(
            events_rx
                .try_iter()
                .any(|event| matches!(event, Event::Player(state) if !state.liked))
        );
        worker.catalog.take();
        worker.catalog_worker.take().unwrap().join().unwrap();
        drop(worker);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn play_all_shuffle_can_select_nonfirst_available_row() {
        let rows = vec![row(1, false), row(2, false), row(1, false)];
        assert_eq!(select_play_all(&rows, false, |_| 0), Some(0));
        assert_eq!(
            select_play_all(&rows, true, |i| if i == 2 { 0 } else { 1 }),
            Some(2)
        );
        let (queue, index) = initial_queue(&rows, 2).unwrap();
        assert_eq!(index, 2);
        assert!(Arc::ptr_eq(&queue[index], &rows[2].track));
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
                    isrc: None,
                })
                .unwrap();
            seed_like(&store, "yandex:1");
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
        let worker = Worker::new(
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
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod sync_tests {
    use super::*;
    use crate::tokens::TokenStore;
    use eagle_core::*;
    struct EditFixture {
        snapshot: PlaylistSnapshot,
        mode: u8,
        calls: std::sync::atomic::AtomicUsize,
    }
    impl Provider for EditFixture {
        fn info(&self) -> &ProviderInfo {
            static INFO: ProviderInfo = ProviderInfo {
                id: "yandex",
                name: "yandex",
                auth: AuthMethod::Token {
                    hint: "",
                    help_url: None,
                },
                capabilities: fixture_capabilities(),
            };
            &INFO
        }
        fn account(&self) -> std::result::Result<Account, ProviderError> {
            Ok(Account {
                id: "1".into(),
                subscription: true,
                capabilities: Capabilities::default(),
            })
        }
        fn login(&self, _: Token) -> std::result::Result<Account, ProviderError> {
            self.account()
        }
        fn search(&self, _: &str, _: u32) -> std::result::Result<Page<Track>, ProviderError> {
            Err(ProviderError::Unavailable)
        }
        fn track(&self, _: &TrackId) -> std::result::Result<Track, ProviderError> {
            Err(ProviderError::Unavailable)
        }
        fn album(&self, _: &TrackId) -> std::result::Result<Album, ProviderError> {
            Err(ProviderError::Unavailable)
        }
        fn playlist(&self, _: &TrackId) -> std::result::Result<Playlist, ProviderError> {
            Ok(self.snapshot.playlist.clone())
        }
        fn playlist_snapshot(
            &self,
            _: &TrackId,
        ) -> std::result::Result<PlaylistSnapshot, ProviderError> {
            if self.mode == 3 {
                return Err(ProviderError::Unavailable);
            }
            Ok(self.snapshot.clone())
        }
        fn playlists(&self, _: Option<&str>) -> std::result::Result<Page<Playlist>, ProviderError> {
            Err(ProviderError::Unavailable)
        }
        fn likes(&self, _: Option<&str>) -> std::result::Result<Page<Track>, ProviderError> {
            Err(ProviderError::Unavailable)
        }
        fn set_liked(&self, _: &TrackId, _: bool) -> std::result::Result<(), ProviderError> {
            Err(ProviderError::Unavailable)
        }
        fn set_disliked(&self, _: &TrackId, _: bool) -> std::result::Result<(), ProviderError> {
            Err(ProviderError::Unavailable)
        }
        fn resolve_stream(
            &self,
            _: &TrackId,
            _: Quality,
        ) -> std::result::Result<Stream, ProviderError> {
            Err(ProviderError::Unavailable)
        }
        fn edit_playlist(
            &self,
            snapshot: &PlaylistSnapshot,
            edit: PlaylistEdit,
        ) -> std::result::Result<PlaylistSnapshot, PlaylistEditError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.mode == 1 {
                return Err(PlaylistEditError::Conflict);
            }
            if self.mode == 2 {
                return Err(PlaylistEditError::Uncertain);
            }
            let mut result = snapshot.clone();
            if let PlaylistEdit::Delete { from, to } = edit {
                result.playlist.tracks.drain(from..to);
            }
            result.revision += 1;
            Ok(result)
        }
    }
    #[test]
    fn created_playlist_loads_snapshot_and_handles_snapshot_failure() {
        for mode in [0, 3] {
            let mut c = catalog(false);
            let playlist = Playlist {
                id: "yandex:1_13".parse().unwrap(),
                title: "new".into(),
                tracks: vec![],
                cover: None,
            };
            c.store
                .put_json(
                    "playlists",
                    &playlist.id.to_string(),
                    serde_json::to_string(&playlist).unwrap(),
                )
                .unwrap();
            c.providers = registry(Arc::new(EditFixture {
                snapshot: PlaylistSnapshot {
                    playlist: playlist.clone(),
                    revision: 7,
                },
                mode,
                calls: std::sync::atomic::AtomicUsize::new(0),
            }));
            let root = c.root.join("worker");
            let (tx, _) = mpsc::channel();
            let mut worker = Worker::new(root.clone(), Arc::new(|_| {}), tx).unwrap();
            worker.catalog.take();
            worker.catalog_worker.take().unwrap().join().unwrap();
            let (catalog_tx, catalog_rx) = mpsc::channel();
            worker.catalog = Some(catalog_tx);
            worker
                .playlist_changed(worker.view_generation, vec![], Some(playlist.clone()), None)
                .unwrap();
            let request = catalog_rx.recv().unwrap();
            assert!(
                matches!(&request, CatalogRequest::Command(_, Command::Playlist(id)) if *id == playlist.id)
            );
            let CatalogReply::Page {
                header,
                playlist_snapshot,
                ..
            } = c.request(request).unwrap()
            else {
                panic!("expected playlist page");
            };
            assert_eq!(header.editable, mode == 0);
            assert_eq!(header.playlist_revision, (mode == 0).then_some(7));
            assert_eq!(playlist_snapshot.is_some(), mode == 0);
            drop(worker);
            std::fs::remove_dir_all(root).unwrap();
        }
    }
    #[test]
    fn playlist_edits_publish_only_success_or_reread_and_never_retry_uncertain() {
        for mode in 0..3 {
            let mut c = catalog(false);
            let track = Track {
                id: "yandex:22".parse().unwrap(),
                title: "duplicate".into(),
                artists: vec![],
                album: None,
                album_title: None,
                duration_ms: 1,
                cover: None,
                availability: Availability::Available,
                isrc: None,
            };
            let snapshot = PlaylistSnapshot {
                playlist: Playlist {
                    id: "yandex:1_13".parse().unwrap(),
                    title: "test".into(),
                    tracks: vec![track.clone(), track],
                    cover: None,
                },
                revision: 8,
            };
            let provider = Arc::new(EditFixture {
                snapshot: snapshot.clone(),
                mode,
                calls: std::sync::atomic::AtomicUsize::new(0),
            });
            c.providers = registry(provider.clone());
            let CatalogReply::PlaylistEdited(Some(result), Some(rows), error) = c
                .request(CatalogRequest::EditPlaylist(
                    1,
                    snapshot,
                    PlaylistEdit::Delete { from: 1, to: 2 },
                ))
                .unwrap()
            else {
                panic!("expected edit result")
            };
            assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert_eq!(result.revision, if mode == 0 { 9 } else { 8 });
            assert_eq!(rows.len(), if mode == 0 { 1 } else { 2 });
            assert_eq!(error.is_some(), mode != 0);
            let json: String = c
                .store
                .read()
                .unwrap()
                .query_row(
                    "SELECT json FROM playlists WHERE id=?1",
                    [result.playlist.id.to_string()],
                    |r| r.get(0),
                )
                .unwrap();
            let stored: Playlist = serde_json::from_str(&json).unwrap();
            assert_eq!(stored.tracks.len(), rows.len());
            let root = c.root.clone();
            drop(c);
            std::fs::remove_dir_all(root).unwrap();
        }
    }
    #[test]
    fn unsupported_snapshot_keeps_own_playlist_read_only() {
        let mut c = catalog(false);
        let id: TrackId = "yandex:1_13".parse().unwrap();
        let snapshot = PlaylistSnapshot {
            playlist: Playlist {
                id: id.clone(),
                title: "test".into(),
                tracks: vec![],
                cover: None,
            },
            revision: 1,
        };
        c.store
            .put_json(
                "playlists",
                &id.to_string(),
                serde_json::to_string(&snapshot.playlist).unwrap(),
            )
            .unwrap();
        c.providers = registry(Arc::new(EditFixture {
            snapshot,
            mode: 3,
            calls: std::sync::atomic::AtomicUsize::new(0),
        }));
        let CatalogReply::Page {
            header,
            playlist_snapshot,
            ..
        } = c.command(Command::Playlist(id)).unwrap()
        else {
            panic!("expected page")
        };
        assert!(header.deletable);
        assert!(!header.editable);
        assert_eq!(header.playlist_revision, None);
        assert!(playlist_snapshot.is_none());
        let root = c.root.clone();
        drop(c);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn uncertain_and_cache_failure_preserve_both_messages() {
        let mut c = catalog(false);
        let snapshot = PlaylistSnapshot {
            playlist: Playlist {
                id: "yandex:1_13".parse().unwrap(),
                title: "test".into(),
                tracks: vec![],
                cover: None,
            },
            revision: 1,
        };
        let provider = Arc::new(EditFixture {
            snapshot: snapshot.clone(),
            mode: 2,
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        c.providers = registry(provider.clone());
        c.store.write(|db| { db.execute_batch("CREATE TRIGGER fail_playlist BEFORE INSERT ON playlists BEGIN SELECT RAISE(ABORT, 'test failure'); END;")?; Ok(()) }).unwrap();
        let CatalogReply::PlaylistEdited(_, None, Some(message)) = c
            .request(CatalogRequest::EditPlaylist(
                1,
                snapshot,
                PlaylistEdit::Delete { from: 0, to: 1 },
            ))
            .unwrap()
        else {
            panic!("expected failure")
        };
        assert!(message.contains("результат изменения неизвестен"));
        assert!(message.contains("локально"));
        assert!(message.contains("вручную"));
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let root = c.root.clone();
        drop(c);
        std::fs::remove_dir_all(root).unwrap();
    }
    pub(super) fn offline_provider() -> Arc<dyn Provider> {
        Arc::new(Fixture {
            fail: true,
            empty: false,
            gate: None,
            calls: Arc::default(),
        })
    }
    /// A registry holding just `provider`, signed in as account "1" with the fixture capabilities.
    fn registry(provider: Arc<dyn Provider>) -> Arc<Providers> {
        let providers = Providers::new(vec![provider], &[]);
        providers.set_account(
            "yandex",
            Some(Account {
                id: "1".into(),
                subscription: true,
                capabilities: fixture_capabilities(),
            }),
        );
        Arc::new(providers)
    }
    /// What the fixtures below implement, so the capability-driven routing reaches them.
    const fn fixture_capabilities() -> Capabilities {
        let mut c = Capabilities::none();
        c.search_tracks = true;
        c.search_artists = true;
        c.search_albums = true;
        c.search_playlists = true;
        c.likes = true;
        c.liked_albums = true;
        c.liked_artists = true;
        c.playlists_read = true;
        c.playlists_edit = true;
        c
    }
    struct Fixture {
        fail: bool,
        empty: bool,
        gate: Option<Arc<(std::sync::Barrier, std::sync::Barrier)>>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Provider for Fixture {
        fn info(&self) -> &ProviderInfo {
            static INFO: ProviderInfo = ProviderInfo {
                id: "yandex",
                name: "yandex",
                auth: AuthMethod::Token {
                    hint: "",
                    help_url: None,
                },
                capabilities: fixture_capabilities(),
            };
            &INFO
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
                    isrc: None,
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
            if self.fail && self.empty {
                return Err(ProviderError::Network);
            }
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
                isrc: None,
            };
            Ok(Page {
                items: vec![track],
                next: (cursor.is_none()).then(|| "next".to_string()),
            })
        }
        fn liked_albums(&self) -> std::result::Result<Vec<Album>, ProviderError> {
            Ok(vec![])
        }
        fn liked_artists(&self) -> std::result::Result<Vec<Artist>, ProviderError> {
            Ok(vec![])
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
        catalog_with(registry(Arc::new(Fixture {
            fail,
            empty: false,
            gate: None,
            calls: Arc::default(),
        })))
    }
    fn catalog_with(providers: Arc<Providers>) -> Catalog {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "eagle-sync-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        Catalog {
            likes_more: std::cell::Cell::new(false),
            clock: wall_clock(),
            covers: RefCell::new(VecDeque::new()),
            last_album: RefCell::new(None),
            last_artist: RefCell::new(None),
            store: Arc::new(Store::open(&root).unwrap()),
            net: Arc::new(eagle_net::Client::new().unwrap()),
            providers,
            root,
        }
    }
    #[test]
    fn track_and_album_commands_reach_the_provider_that_owns_the_id() {
        use eagle_provider_sdk::testkit::{FakeProvider, fake_track};
        let fakea = Arc::new(FakeProvider::new("fakea"));
        let mut track = fake_track("fakeb", "1", "Song", "Artist", 1000);
        track.album = Some("fakeb:al1".parse().unwrap());
        track.album_title = Some("Album".into());
        let fakeb = Arc::new(FakeProvider::new("fakeb").with_tracks(vec![track]));
        let all: Vec<Arc<dyn Provider>> = vec![fakea.clone(), fakeb.clone()];
        let providers = Arc::new(Providers::new(all, &[]));
        providers.set_account("fakea", Some(fakea.login(Token::new("a".into())).unwrap()));
        providers.set_account("fakeb", Some(fakeb.login(Token::new("b".into())).unwrap()));
        let c = catalog_with(providers);
        c.store
            .put_track(&fake_track("fakeb", "1", "Song", "Artist", 1000))
            .unwrap();

        let id: TrackId = "fakeb:1".parse().unwrap();
        c.request(CatalogRequest::Command(0, Command::Like(id.clone())))
            .unwrap();
        assert_eq!(fakeb.set_liked_calls(), vec![(id.clone(), true)]);
        assert!(fakea.set_liked_calls().is_empty());

        let CatalogReply::Page { items, title, .. } = c
            .request(CatalogRequest::Command(
                0,
                Command::Album("fakeb:al1".parse().unwrap()),
            ))
            .unwrap()
        else {
            panic!("album page expected");
        };
        assert_eq!(title, "Album");
        assert_eq!(items[0].track.id, id);

        let unknown = c.request(CatalogRequest::Command(
            0,
            Command::Album("nope:al1".parse().unwrap()),
        ));
        assert!(unknown.is_err());
    }
    fn two_search_providers(
        slow: Duration,
        failing: (bool, bool),
    ) -> (
        Catalog,
        Arc<eagle_provider_sdk::testkit::FakeProvider>,
        Arc<eagle_provider_sdk::testkit::FakeProvider>,
    ) {
        use eagle_provider_sdk::testkit::{FakeProvider, fake_track};
        let fakea = FakeProvider::new("fakea").with_tracks(vec![
            fake_track("fakea", "1", "Song", "Artist", 180_000),
            fake_track("fakea", "2", "Only A", "Artist", 100_000),
        ]);
        let fakeb = FakeProvider::new("fakeb")
            .with_tracks(vec![
                fake_track("fakeb", "9", "Song", "Artist", 180_500),
                fake_track("fakeb", "8", "Only B", "Other", 90_000),
            ])
            .with_delay(slow);
        let fail = || ProviderError::Network;
        let fakea = Arc::new(if failing.0 {
            fakea.fail_with(fail)
        } else {
            fakea
        });
        let fakeb = Arc::new(if failing.1 {
            fakeb.fail_with(fail)
        } else {
            fakeb
        });
        let all: Vec<Arc<dyn Provider>> = vec![fakea.clone(), fakeb.clone()];
        let providers = Arc::new(Providers::new(all, &[]));
        providers.set_account("fakea", Some(fakea.login(Token::new("a".into())).unwrap()));
        providers.set_account("fakeb", Some(fakeb.login(Token::new("b".into())).unwrap()));
        (catalog_with(providers), fakea, fakeb)
    }
    #[test]
    fn search_merges_copies_and_reports_first_results_before_the_slow_provider() {
        let (c, _, fakeb) = two_search_providers(Duration::from_millis(150), (false, false));
        let partials = std::cell::RefCell::new(Vec::new());
        let reply = c
            .search_merged("", &|partial| {
                let CatalogReply::Tracks(items, _) = partial else {
                    panic!("tracks expected");
                };
                partials
                    .borrow_mut()
                    .push((items.len(), fakeb.search_calls()));
            })
            .unwrap();
        // The only partial arrives while the slow provider has not been asked yet.
        assert_eq!(*partials.borrow(), vec![(2, 0)]);
        let CatalogReply::Tracks(items, _) = reply else {
            panic!("tracks expected");
        };
        let ids: Vec<_> = items.iter().map(|i| i.track.id.to_string()).collect();
        assert_eq!(ids, ["fakea:1", "fakea:2", "fakeb:8"]);
        assert_eq!(items[0].sources, ["fakeb:9".parse::<TrackId>().unwrap()]);
        assert!(items[1].sources.is_empty());
        // Both copies were stored, so playback can fall back to the hidden one.
        let grp = c.store.group_of(&items[0].track.id).unwrap().unwrap();
        assert_eq!(c.store.group_members(grp).unwrap().len(), 2);
    }
    #[test]
    fn search_survives_one_failing_provider_and_fails_when_all_fail() {
        let (c, _, _) = two_search_providers(Duration::ZERO, (false, true));
        let CatalogReply::Tracks(items, _) = c.search_merged("", &|_| {}).unwrap() else {
            panic!("tracks expected");
        };
        assert_eq!(items.len(), 2);
        let (c, _, _) = two_search_providers(Duration::ZERO, (true, true));
        assert!(c.search_merged("", &|_| {}).is_err());
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
    /// Two signed-in fakes in the given priority `order`; `failing` injects a network error.
    fn card_search_pair(
        a: eagle_provider_sdk::testkit::FakeProvider,
        b: eagle_provider_sdk::testkit::FakeProvider,
        order: &[&str],
        failing: (bool, bool),
    ) -> (
        Catalog,
        Arc<eagle_provider_sdk::testkit::FakeProvider>,
        Arc<eagle_provider_sdk::testkit::FakeProvider>,
    ) {
        let fail = || ProviderError::Network;
        let a = Arc::new(if failing.0 { a.fail_with(fail) } else { a });
        let b = Arc::new(if failing.1 { b.fail_with(fail) } else { b });
        let all: Vec<Arc<dyn Provider>> = vec![a.clone(), b.clone()];
        let order: Vec<String> = order.iter().map(|id| (*id).to_string()).collect();
        let providers = Arc::new(Providers::new(all, &order));
        providers.set_account("fakea", Some(a.login(Token::new("a".into())).unwrap()));
        providers.set_account("fakeb", Some(b.login(Token::new("b".into())).unwrap()));
        (catalog_with(providers), a, b)
    }
    #[test]
    fn artist_search_merges_the_same_artist_across_providers() {
        use eagle_provider_sdk::testkit::{FakeProvider, fake_track};
        let a = FakeProvider::new("fakea")
            .with_tracks(vec![fake_track("fakea", "1", "Song", "Same", 180_000)]);
        let b = FakeProvider::new("fakeb")
            .with_tracks(vec![fake_track("fakeb", "9", "Song", "Same", 180_500)]);
        let (c, _, _) = card_search_pair(a, b, &["fakea", "fakeb"], (false, false));
        // The same recording on both providers joins the two artist ids into one group.
        c.store
            .put_track(&fake_track("fakea", "1", "Song", "Same", 180_000))
            .unwrap();
        c.store
            .put_track(&fake_track("fakeb", "9", "Song", "Same", 180_500))
            .unwrap();
        let CatalogReply::Cards(cards) = c
            .search_cards_merged(CardKind::Artists, "same", &|_| {})
            .unwrap()
        else {
            panic!("cards expected");
        };
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].id.to_string(), "fakea:artist-same");
        assert_eq!(
            cards[0].sources,
            ["fakeb:artist-same".parse::<TrackId>().unwrap()]
        );
    }
    #[test]
    fn namesakes_without_a_shared_track_stay_separate() {
        use eagle_provider_sdk::testkit::{FakeProvider, fake_track};
        let a = FakeProvider::new("fakea")
            .with_tracks(vec![fake_track("fakea", "1", "Solo A", "Same", 180_000)]);
        let b = FakeProvider::new("fakeb")
            .with_tracks(vec![fake_track("fakeb", "9", "Solo B", "Same", 240_000)]);
        let (c, _, _) = card_search_pair(a, b, &["fakea", "fakeb"], (false, false));
        c.store
            .put_track(&fake_track("fakea", "1", "Solo A", "Same", 180_000))
            .unwrap();
        c.store
            .put_track(&fake_track("fakeb", "9", "Solo B", "Same", 240_000))
            .unwrap();
        let CatalogReply::Cards(cards) = c
            .search_cards_merged(CardKind::Artists, "same", &|_| {})
            .unwrap()
        else {
            panic!("cards expected");
        };
        assert_eq!(cards.len(), 2);
        assert!(cards.iter().all(|card| card.sources.is_empty()));
    }
    #[test]
    fn albums_and_playlists_are_listed_by_provider_priority_without_merging() {
        use eagle_provider_sdk::testkit::{FakeProvider, fake_track};
        let album = |provider: &str| {
            let mut track = fake_track(provider, "1", "Song", "Artist", 1000);
            track.album = Some(format!("{provider}:al1").parse().unwrap());
            track.album_title = Some("Same Album".into());
            track
        };
        let playlist = |provider: &str| Playlist {
            id: format!("{provider}:pl1").parse().unwrap(),
            title: "Same Mix".into(),
            tracks: vec![],
            cover: None,
        };
        let a = FakeProvider::new("fakea")
            .with_tracks(vec![album("fakea")])
            .with_playlists(vec![playlist("fakea")]);
        let b = FakeProvider::new("fakeb")
            .with_tracks(vec![album("fakeb")])
            .with_playlists(vec![playlist("fakeb")]);
        // Same titles on both providers: the cards stay apart.
        let (c, _, _) = card_search_pair(a, b, &["fakeb", "fakea"], (false, false));
        let ids = |kind| {
            let CatalogReply::Cards(cards) = c.search_cards_merged(kind, "same", &|_| {}).unwrap()
            else {
                panic!("cards expected");
            };
            cards
                .iter()
                .map(|card| card.id.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(CardKind::Albums), ["fakeb:al1", "fakea:al1"]);
        assert_eq!(ids(CardKind::Playlists), ["fakeb:pl1", "fakea:pl1"]);
    }
    #[test]
    fn card_search_survives_one_failing_provider() {
        use eagle_provider_sdk::testkit::{FakeProvider, fake_track};
        let artists = || {
            (
                FakeProvider::new("fakea")
                    .with_tracks(vec![fake_track("fakea", "1", "Song", "Same", 1000)]),
                FakeProvider::new("fakeb")
                    .with_tracks(vec![fake_track("fakeb", "9", "Song", "Other", 1000)]),
            )
        };
        let (a, b) = artists();
        let (c, _, _) = card_search_pair(a, b, &["fakea", "fakeb"], (true, false));
        let CatalogReply::Cards(cards) = c
            .search_cards_merged(CardKind::Artists, "", &|_| {})
            .unwrap()
        else {
            panic!("cards expected");
        };
        let ids: Vec<_> = cards.iter().map(|card| card.id.to_string()).collect();
        assert_eq!(ids, ["fakeb:artist-other"]);
        let (a, b) = artists();
        let (c, _, _) = card_search_pair(a, b, &["fakea", "fakeb"], (true, true));
        assert!(
            c.search_cards_merged(CardKind::Artists, "", &|_| {})
                .is_err()
        );
    }
    #[test]
    fn card_search_reports_first_results_before_the_slow_provider() {
        use eagle_provider_sdk::testkit::{FakeProvider, fake_track};
        let a = FakeProvider::new("fakea")
            .with_tracks(vec![fake_track("fakea", "1", "Song", "Same", 1000)]);
        let b = FakeProvider::new("fakeb")
            .with_tracks(vec![fake_track("fakeb", "9", "Song", "Other", 1000)])
            .with_delay(Duration::from_millis(200));
        let (c, _, _) = card_search_pair(a, b, &["fakea", "fakeb"], (false, false));
        let partials = std::cell::RefCell::new(Vec::new());
        let reply = c
            .search_cards_merged(CardKind::Artists, "", &|partial| {
                let CatalogReply::Cards(cards) = partial else {
                    panic!("cards expected");
                };
                partials.borrow_mut().push(cards.len());
            })
            .unwrap();
        // The only partial arrives before the slow provider has answered.
        assert_eq!(*partials.borrow(), vec![1]);
        let CatalogReply::Cards(cards) = reply else {
            panic!("cards expected");
        };
        assert_eq!(cards.len(), 2);
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
        c.providers = registry(Arc::new(Fixture {
            fail: true,
            empty: false,
            gate: None,
            calls: Arc::default(),
        }));
        assert!(c.command(Command::DeletePlaylist(id.clone())).is_err());
        assert!(
            c.command(Command::CreatePlaylist("failure".into()))
                .is_err()
        );
        assert!(
            matches!(c.command(Command::Collections).unwrap(), CatalogReply::Collections(items) if items.len() == 1)
        );
        c.providers = registry(Arc::new(Fixture {
            fail: false,
            empty: false,
            gate: None,
            calls: Arc::default(),
        }));
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
    fn account_switch_clears_only_private_cache_and_preserves_same_account() {
        let c = catalog(false);
        seed_like(&c.store, "yandex:1");
        c.store
            .put_json("playlists", "yandex:1_2", "{}".into())
            .unwrap();
        c.store
            .put_setting(&liked_albums_key("yandex"), "[]".into())
            .unwrap();
        c.store
            .put_setting(&liked_artists_key("yandex"), "[]".into())
            .unwrap();
        c.store.put_setting("unrelated", "kept".into()).unwrap();
        assert!(c.switch_account("1").unwrap());
        let db = c.store.read().unwrap();
        // Likes belong to the profile: a new account does not remove them.
        assert_eq!(
            db.query_row("SELECT count(*) FROM liked", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM playlists", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert!(
            c.store
                .setting(&liked_albums_key("yandex"))
                .unwrap()
                .is_none()
        );
        assert!(
            c.store
                .setting(&liked_artists_key("yandex"))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            c.store.setting("unrelated").unwrap().as_deref(),
            Some("kept")
        );
        seed_like(&c.store, "yandex:2");
        assert!(!c.switch_account("1").unwrap());
        assert_eq!(
            db.query_row("SELECT count(*) FROM liked", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert!(c.switch_account("2").unwrap());
        assert_eq!(
            db.query_row("SELECT count(*) FROM liked", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
    }
    #[test]
    fn playlist_failure_does_not_block_likes_and_collection_events() {
        let mut c = catalog(false);
        c.providers = registry(Arc::new(Fixture {
            fail: true,
            empty: true,
            gate: None,
            calls: Arc::default(),
        }));
        seed_synced_like(&c.store, "yandex:99");
        let mut state = SyncState::default();
        assert!(c.sync_step(&mut state).unwrap().0.is_none());
        assert_eq!(state.step, 1);
        c.sync_step(&mut state).unwrap();
        assert!(matches!(
            c.sync_step(&mut state).unwrap().0,
            Some(CatalogReply::Synced)
        ));
        assert!(matches!(
            c.sync_step(&mut state).unwrap().0,
            Some(CatalogReply::AlbumsSynced)
        ));
        let (reply, done) = c.sync_step(&mut state).unwrap();
        assert!(done && matches!(reply, Some(CatalogReply::ArtistsSynced)));
        assert_eq!(
            c.store
                .read()
                .unwrap()
                .query_row("SELECT count(*) FROM liked", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    #[test]
    fn likes_publication_failure_still_syncs_albums_and_artists() {
        let c = catalog(false);
        c.store
            .write(|db| {
                db.execute("DROP TABLE provider_liked_seen", [])?;
                Ok(())
            })
            .unwrap();
        let mut state = SyncState {
            step: 2,
            provider: c.providers.first_with(|caps| caps.likes).cloned(),
            ..SyncState::default()
        };
        assert!(c.sync_step(&mut state).unwrap().0.is_none());
        assert_eq!(state.step, 3);
        assert!(matches!(
            c.sync_step(&mut state).unwrap().0,
            Some(CatalogReply::AlbumsSynced)
        ));
        assert!(matches!(
            c.sync_step(&mut state).unwrap().0,
            Some(CatalogReply::ArtistsSynced)
        ));
    }
    #[test]
    fn catalog_drains_commands_and_cancels_old_account_sync() {
        let mut c = catalog(false);
        let gate = Arc::new((std::sync::Barrier::new(2), std::sync::Barrier::new(2)));
        c.providers = registry(Arc::new(Fixture {
            fail: false,
            empty: false,
            gate: Some(gate.clone()),
            calls: Arc::default(),
        }));
        seed_like(&c.store, "yandex:99");
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
            Command::Login {
                provider: "yandex".into(),
                token: Token::new("new".into()),
            },
        ))
        .unwrap();
        gate.1.wait();
        let released = Instant::now();
        let mut seen = Vec::new();
        while seen.len() < 2 {
            if let Message::Catalog(generation, _) =
                reply_rx.recv_timeout(Duration::from_secs(5)).unwrap()
                && generation != 0
            {
                if generation == 10 {
                    println!(
                        "catalog command latency after page release: {:?}",
                        released.elapsed()
                    );
                }
                seen.push(generation);
            }
        }
        assert_eq!(seen, [10, 11]);
        drop(tx);
        worker.join().unwrap();
        let count: u32 = store
            .read()
            .unwrap()
            .query_row("SELECT count(*) FROM liked l JOIN track_groups g ON g.grp=l.grp WHERE g.track='yandex:99'", [], |r| {
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
        c.providers = registry(Arc::new(Fixture {
            fail: false,
            empty: false,
            gate: Some(gate.clone()),
            calls: calls.clone(),
        }));
        seed_like(&c.store, "yandex:99");
        seed_track(&c.store, "yandex:3");
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
                Message::Catalog(10, reply) if matches!(*reply, Ok(CatalogReply::Liked(_, _))) => {
                    liked = true
                }
                Message::Catalog(_, reply)
                    if matches!(
                        *reply,
                        Ok(CatalogReply::Synced | CatalogReply::SyncedOffer(..))
                    ) =>
                {
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
        c.providers = registry(Arc::new(Fixture {
            fail: false,
            empty: false,
            gate: Some(gate.clone()),
            calls: calls.clone(),
        }));
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
            if let Message::Catalog(_, reply) =
                reply_rx.recv_timeout(Duration::from_secs(5)).unwrap()
                && matches!(*reply, Ok(CatalogReply::Synced))
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
        c.providers = registry(Arc::new(Fixture {
            fail: false,
            empty: true,
            gate: None,
            calls: Arc::default(),
        }));
        seed_synced_like(&c.store, "yandex:99");
        let mut state = SyncState::default();
        c.sync_step(&mut state).unwrap();
        c.sync_step(&mut state).unwrap();
        let count = || {
            c.store
                .read()
                .unwrap()
                .query_row("SELECT count(*) FROM liked", [], |r| r.get::<_, u32>(0))
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
            seed_synced_like(&c.store, "yandex:99");
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
                    .query_row("SELECT count(*) FROM liked l JOIN track_groups g ON g.grp=l.grp WHERE g.track='yandex:99'", [], |r| {
                        r.get::<_, u32>(0)
                    })
                    .unwrap()
            };
            assert_eq!(count(), 1);
            if fail {
                assert!(c.sync_step(&mut state).unwrap().0.is_none());
                assert_eq!(state.step, 3);
                assert_eq!(count(), 1);
                assert!(matches!(
                    c.sync_step(&mut state).unwrap().0,
                    Some(CatalogReply::AlbumsSynced)
                ));
                let (reply, done) = c.sync_step(&mut state).unwrap();
                assert!(done && matches!(reply, Some(CatalogReply::ArtistsSynced)));
                assert_eq!(count(), 1);
            } else {
                c.sync_step(&mut state).unwrap();
                assert_eq!(count(), 1);
                c.sync_step(&mut state).unwrap();
                assert_eq!(count(), 0);
                let ids: Vec<String> = c
                    .store
                    .liked_tracks(&[])
                    .unwrap()
                    .into_iter()
                    .map(|track| track.id.to_string())
                    .collect();
                assert_eq!(ids, ["yandex:1", "yandex:2"]);
            }
        }
    }
    fn token_providers() -> (
        Arc<Providers>,
        Arc<crate::tokens::MemoryTokens>,
        Arc<eagle_provider_sdk::testkit::FakeProvider>,
        Arc<eagle_provider_sdk::testkit::FakeProvider>,
    ) {
        use eagle_provider_sdk::testkit::FakeProvider;
        let tokens = Arc::new(crate::tokens::MemoryTokens::default());
        let fakea = Arc::new(FakeProvider::new("fakea").signed_out());
        let fakeb = Arc::new(FakeProvider::new("fakeb").signed_out());
        let all: Vec<Arc<dyn Provider>> = vec![fakea.clone(), fakeb.clone()];
        let providers = Arc::new(Providers::new(all, &[]).with_tokens(tokens.clone()));
        (providers, tokens, fakea, fakeb)
    }
    #[test]
    fn login_signs_in_one_provider_and_only_the_account_provider_sets_the_account() {
        let (providers, tokens, _, _) = token_providers();
        let c = catalog_with(providers.clone());
        let login = |provider: &str, token: &str| {
            c.command(Command::Login {
                provider: provider.into(),
                token: Token::new(token.into()),
            })
        };
        assert!(matches!(
            login("fakeb", "b").unwrap(),
            CatalogReply::Providers
        ));
        assert!(tokens.load("fakeb").unwrap().is_some());
        assert!(c.store.setting(&account_key("fakeb")).unwrap().is_none());
        assert!(c.store.setting(&account_key("fakea")).unwrap().is_none());
        assert!(!providers.statuses()[0].signed_in && providers.statuses()[1].signed_in);

        assert!(matches!(
            login("fakea", "a").unwrap(),
            CatalogReply::Account(true)
        ));
        assert_eq!(
            c.store.setting(&account_key("fakea")).unwrap().as_deref(),
            Some("fake-uid")
        );
        // A rejected token is not saved, and an unknown provider is an error.
        assert!(login("fakea", "").is_err());
        assert!(login("nope", "x").is_err());
    }
    #[test]
    fn restore_checks_every_saved_token_and_a_bad_one_does_not_block_the_rest() {
        let (providers, tokens, _, _) = token_providers();
        tokens.save("fakeb", &Token::new(String::new())).unwrap();
        tokens.save("fakea", &Token::new("a".into())).unwrap();
        let c = catalog_with(providers.clone());
        assert!(matches!(
            c.command(Command::Restore).unwrap(),
            CatalogReply::Account(true)
        ));
        let status = providers.statuses();
        assert!(status[0].signed_in && !status[1].signed_in);
        assert!(status[0].capabilities.likes && !status[1].capabilities.likes);
        // Nothing saved anywhere: no account change, but the UI still gets the state.
        let (providers, _, _, _) = token_providers();
        let c = catalog_with(providers);
        assert!(matches!(
            c.command(Command::Restore).unwrap(),
            CatalogReply::Providers
        ));
    }
    #[test]
    fn app_starts_with_no_signed_in_providers() {
        let root = std::env::temp_dir().join(format!("eg-fresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (_, tokens, fakea, fakeb) = token_providers();
        let emit: Arc<dyn Fn(Event) + Send + Sync> = Arc::new(|_| {});
        let (tx, _) = mpsc::channel();
        let worker = Worker::new_with_providers(
            root.clone(),
            emit,
            tx,
            Some(vec![fakea, fakeb]),
            tokens,
            wall_clock(),
        )
        .unwrap();
        let status = worker.providers.statuses();
        assert_eq!(status.len(), 2);
        assert!(status.iter().all(|p| !p.signed_in));
        assert!(worker.account.is_empty());
        assert_eq!(worker.providers.signed_in().count(), 0);
        drop(worker);
        let _ = std::fs::remove_dir_all(root);
    }
    fn logout_worker(
        tag: &str,
    ) -> (
        Worker,
        Arc<crate::tokens::MemoryTokens>,
        Arc<std::sync::Mutex<Vec<Vec<(String, bool)>>>>,
    ) {
        use eagle_provider_sdk::testkit::fake_track;
        let root = std::env::temp_dir().join(format!("eg-logout-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (_, tokens, fakea, fakeb) = token_providers();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        let emit: Arc<dyn Fn(Event) + Send + Sync> = Arc::new(move |event| {
            if let Event::Providers(all) = event {
                sink.lock()
                    .unwrap()
                    .push(all.into_iter().map(|p| (p.id, p.signed_in)).collect());
            }
        });
        let (tx, _) = mpsc::channel();
        let mut worker = Worker::new_with_providers(
            root,
            emit,
            tx,
            Some(vec![fakea.clone(), fakeb.clone()]),
            tokens.clone(),
            wall_clock(),
        )
        .unwrap();
        for (provider, token) in [(&fakea, "a"), (&fakeb, "b")] {
            worker
                .providers
                .sign_in(provider.as_ref(), Token::new(token.into()))
                .unwrap();
        }
        worker.account = "fake-uid".into();
        worker
            .store
            .write(|c| {
                c.execute(
                    "INSERT INTO settings VALUES('account:fakea','fake-uid')",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        for (provider, id) in [("fakea", "1"), ("fakeb", "2")] {
            let track = fake_track(provider, id, "Song", "Artist", 1000);
            worker.store.put_track(&track).unwrap();
            worker.store.like(&track.id, true, &[]).unwrap();
            worker
                .store
                .enqueue_download("fake-uid", &track.id, Quality::High)
                .unwrap();
            let job = worker.store.claim_download("fake-uid").unwrap().unwrap();
            assert!(worker.store.finish_saved_download(&job).unwrap());
        }
        // Only now may the download worker look at this account's queue.
        worker.downloads.account("fake-uid".into());
        (worker, tokens, seen)
    }
    fn saved(worker: &Worker) -> Vec<String> {
        worker
            .store
            .downloads("fake-uid")
            .unwrap()
            .into_iter()
            .filter(|job| job.state == DownloadState::Saved)
            .map(|job| job.track.to_string())
            .collect()
    }
    #[test]
    fn logout_forgets_one_provider_and_keeps_likes_and_the_others_data() {
        let (mut worker, tokens, seen) = logout_worker("forget");
        assert_eq!(saved(&worker), ["fakea:1", "fakeb:2"]);
        worker
            .command(Command::Logout {
                provider: "fakea".into(),
                keep_saved: false,
            })
            .unwrap();
        assert!(tokens.load("fakea").unwrap().is_none());
        assert!(tokens.load("fakeb").unwrap().is_some());
        assert!(worker.store.setting("account:fakea").unwrap().is_none());
        assert!(worker.account.is_empty());
        assert_eq!(saved(&worker), ["fakeb:2"]);
        let mut likes: Vec<String> = worker
            .store
            .liked_track_ids()
            .unwrap()
            .into_iter()
            .collect();
        likes.sort();
        assert_eq!(likes, ["fakea:1", "fakeb:2"]);
        let last = seen.lock().unwrap().last().cloned().unwrap();
        assert_eq!(
            last,
            [("fakea".to_owned(), false), ("fakeb".to_owned(), true)]
        );
        assert!(
            worker
                .command(Command::Logout {
                    provider: "nope".into(),
                    keep_saved: false
                })
                .is_err()
        );
    }
    /// Two signed-in fake providers that both know the song, behind a catalog.
    fn like_catalog(
        failure: Option<fn() -> ProviderError>,
    ) -> (
        Catalog,
        Arc<eagle_provider_sdk::testkit::FakeProvider>,
        Arc<eagle_provider_sdk::testkit::FakeProvider>,
    ) {
        use eagle_provider_sdk::testkit::{FakeProvider, fake_track};
        let song = |provider| fake_track(provider, "1", "Song", "Artist", 1000);
        let mut fakea = FakeProvider::new("fakea").with_tracks(vec![song("fakea")]);
        if let Some(failure) = failure {
            fakea = fakea.fail_with(failure);
        }
        let fakea = Arc::new(fakea);
        let fakeb = Arc::new(FakeProvider::new("fakeb").with_tracks(vec![song("fakeb")]));
        let all: Vec<Arc<dyn Provider>> = vec![fakea.clone(), fakeb.clone()];
        let providers = Arc::new(Providers::new(all, &[]));
        for provider in [&fakea, &fakeb] {
            providers.set_account(
                provider.info().id,
                Some(provider.login(Token::new("t".into())).unwrap()),
            );
        }
        let c = catalog_with(providers);
        c.store.put_tracks(&[song("fakea"), song("fakeb")]).unwrap();
        (c, fakea, fakeb)
    }
    fn pending_likes(c: &Catalog, provider: &str) -> usize {
        c.store.like_jobs(provider, i64::MAX).unwrap().len()
    }
    fn backfill_catalog() -> (Catalog, Arc<eagle_provider_sdk::testkit::FakeProvider>) {
        use eagle_provider_sdk::testkit::{FakeProvider, fake_track};
        let song = |provider: &str, n: u64| {
            fake_track(
                provider,
                &n.to_string(),
                &format!("Song {n}"),
                "Artist",
                1000 * n,
            )
        };
        let fakea = Arc::new(
            FakeProvider::new("fakea").with_tracks((1..=4).map(|n| song("fakea", n)).collect()),
        );
        let fakeb = Arc::new(
            FakeProvider::new("fakeb").with_tracks((1..=4).map(|n| song("fakeb", n)).collect()),
        );
        let all: Vec<Arc<dyn Provider>> = vec![fakea.clone(), fakeb.clone()];
        let providers = Arc::new(Providers::new(all, &[]));
        for provider in [&fakea, &fakeb] {
            providers.set_account(
                provider.info().id,
                Some(provider.login(Token::new("t".into())).unwrap()),
            );
        }
        let c = catalog_with(providers);
        c.store
            .put_tracks(&[song("fakea", 4), song("fakeb", 4)])
            .unwrap();
        for n in 1..=3 {
            let title = format!("Song {n}");
            let both = [
                fake_track("fakea", &n.to_string(), &title, "Artist", 1000 * n),
                fake_track("fakeb", &n.to_string(), &title, "Artist", 1000 * n),
            ];
            c.store.put_tracks(&both).unwrap();
            c.store.like(&both[0].id, true, &[]).unwrap();
        }
        (c, fakeb)
    }
    fn sync_likes_of(c: &Catalog, provider: &str) -> CatalogReply {
        let mut state = SyncState {
            step: 1,
            done: c
                .providers
                .with_capability(|c| c.likes)
                .map(|p| p.info().id.to_owned())
                .filter(|id| id != provider)
                .collect(),
            ..SyncState::default()
        };
        let mut last = None;
        while state.step != 3 {
            if let (Some(reply), _) = c.sync_step(&mut state).unwrap() {
                last = Some(reply);
            }
        }
        last.unwrap()
    }
    #[test]
    fn old_likes_are_offered_to_a_new_provider_and_sent_only_after_consent() {
        let (c, fakeb) = backfill_catalog();
        // fakea already has the likes; fakeb does not.
        c.store
            .sync_provider_likes(
                "fakea",
                &(1..=3)
                    .map(|n| format!("fakea:{n}").parse().unwrap())
                    .collect::<Vec<_>>(),
            )
            .unwrap();
        assert!(matches!(
            sync_likes_of(&c, "fakeb"),
            CatalogReply::SyncedOffer(p, 3) if p == "fakeb"
        ));
        assert!(fakeb.set_liked_calls().is_empty());
        assert_eq!(pending_likes(&c, "fakeb"), 0);

        c.command(Command::LikesBackfill {
            provider: "fakeb".into(),
            accept: true,
        })
        .unwrap();
        assert_eq!(fakeb.set_liked_calls().len(), 3);
        assert_eq!(pending_likes(&c, "fakeb"), 0);
        // Answered: no second offer, and a like made now goes out at once.
        assert!(matches!(sync_likes_of(&c, "fakeb"), CatalogReply::Synced));
    }
    #[test]
    fn declining_the_backfill_sends_nothing_and_is_not_asked_again() {
        let (c, fakeb) = backfill_catalog();
        c.command(Command::LikesBackfill {
            provider: "fakeb".into(),
            accept: false,
        })
        .unwrap();
        assert!(matches!(sync_likes_of(&c, "fakeb"), CatalogReply::Synced));
        assert!(fakeb.set_liked_calls().is_empty());
        assert_eq!(pending_likes(&c, "fakeb"), 0);
        // New likes still reach the provider.
        let new_like: TrackId = "fakea:4".parse().unwrap();
        c.command(Command::Like(new_like)).unwrap();
        assert_eq!(
            fakeb.set_liked_calls(),
            vec![("fakeb:4".parse().unwrap(), true)]
        );
    }
    #[test]
    fn there_is_no_offer_without_likes_to_send() {
        let (c, _) = backfill_catalog();
        let (c2, _, _) = like_catalog(None);
        assert!(matches!(sync_likes_of(&c2, "fakeb"), CatalogReply::Synced));
        drop(c);
    }
    /// A catalog where `fakea` knows tracks 1..=4, with `unresolved` seeded the way the v13
    /// migration leaves likes whose track metadata is missing.
    fn unresolved_catalog(
        unresolved: &[&str],
    ) -> (Catalog, Arc<eagle_provider_sdk::testkit::FakeProvider>) {
        use eagle_provider_sdk::testkit::{FakeProvider, fake_track};
        let fakea = Arc::new(
            FakeProvider::new("fakea").with_tracks(
                (1..=4)
                    .map(|n| fake_track("fakea", &n.to_string(), "Song", "Artist", 1000))
                    .collect(),
            ),
        );
        let providers = Arc::new(Providers::new(
            vec![fakea.clone() as Arc<dyn Provider>],
            &[],
        ));
        providers.set_account(
            fakea.info().id,
            Some(fakea.login(Token::new("t".into())).unwrap()),
        );
        let c = catalog_with(providers);
        let ids: Vec<String> = unresolved.iter().map(|id| id.to_string()).collect();
        c.store
            .write(move |conn| {
                for id in &ids {
                    conn.execute("INSERT INTO likes_unresolved VALUES(?1, 0)", [id])?;
                }
                Ok(())
            })
            .unwrap();
        (c, fakea)
    }
    #[test]
    fn sync_fetches_metadata_for_unresolved_likes() {
        let (c, _) = unresolved_catalog(&["fakea:3"]);
        sync_likes_of(&c, "fakea");
        assert!(c.store.is_liked(&"fakea:3".parse().unwrap()).unwrap());
        assert_eq!(c.store.unresolved_likes_count().unwrap(), 0);
    }
    #[test]
    fn unresolved_likes_that_stay_missing_are_asked_once_per_sync() {
        let (c, fakea) = unresolved_catalog(&["fakea:404", "gone:7"]);
        sync_likes_of(&c, "fakea");
        assert_eq!(
            fakea.track_calls(),
            vec!["fakea:404".parse::<TrackId>().unwrap()]
        );
        assert_eq!(c.store.unresolved_likes_count().unwrap(), 2);
        // Kept for the next sync, which asks again, once.
        sync_likes_of(&c, "fakea");
        assert_eq!(fakea.track_calls().len(), 2);
        assert_eq!(
            c.store.unresolved_likes("gone", 10).unwrap(),
            vec!["gone:7".parse::<TrackId>().unwrap()]
        );
    }
    #[test]
    fn like_goes_to_every_provider_that_has_the_song() {
        let (c, fakea, fakeb) = like_catalog(None);
        let id: TrackId = "fakea:1".parse().unwrap();
        assert!(matches!(
            c.command(Command::Like(id.clone())).unwrap(),
            CatalogReply::Liked(_, true)
        ));
        assert_eq!(fakea.set_liked_calls(), vec![(id, true)]);
        assert_eq!(
            fakeb.set_liked_calls(),
            vec![("fakeb:1".parse().unwrap(), true)]
        );
        assert_eq!(pending_likes(&c, "fakea") + pending_likes(&c, "fakeb"), 0);
    }
    #[test]
    fn an_offline_provider_keeps_the_like_queued_and_gets_it_later() {
        let (c, fakea, fakeb) = like_catalog(Some(|| ProviderError::Network));
        let id: TrackId = "fakeb:1".parse().unwrap();
        // The like is local at once and reaches the reachable provider.
        assert!(matches!(
            c.command(Command::Like(id.clone())).unwrap(),
            CatalogReply::Liked(_, true)
        ));
        assert!(c.store.is_liked(&id).unwrap());
        assert_eq!(fakeb.set_liked_calls().len(), 1);
        assert_eq!(pending_likes(&c, "fakea"), 1);
        fakea.clear_failure();
        // The failed attempt backs off, so a plain flush does not resend it before its retry
        // time; the network coming back does.
        c.request(CatalogRequest::FlushLikes { online: true })
            .unwrap();
        assert_eq!(pending_likes(&c, "fakea"), 0);
        assert_eq!(fakea.set_liked_calls().len(), 1);
    }
    #[test]
    fn an_uncertain_like_is_not_resent_and_a_rate_limit_waits() {
        let (c, fakea, _) = like_catalog(Some(|| ProviderError::Uncertain));
        c.command(Command::Like("fakea:1".parse().unwrap()))
            .unwrap();
        assert_eq!(pending_likes(&c, "fakea"), 0);
        assert_eq!(c.store.uncertain_like_jobs("fakea").unwrap().len(), 1);
        fakea.clear_failure();
        c.flush_likes();
        assert!(fakea.set_liked_calls().is_empty());

        let (c, fakea, _) = like_catalog(Some(|| ProviderError::RateLimited));
        c.command(Command::Like("fakea:1".parse().unwrap()))
            .unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!(c.store.like_jobs("fakea", now).unwrap().is_empty());
        assert_eq!(pending_likes(&c, "fakea"), 1);
        fakea.clear_failure();
        c.flush_likes();
        assert_eq!(
            pending_likes(&c, "fakea"),
            1,
            "deferred until the retry time"
        );
    }
    #[test]
    fn logout_with_keep_saved_leaves_offline_copies() {
        let (mut worker, tokens, _) = logout_worker("keep");
        worker
            .command(Command::Logout {
                provider: "fakea".into(),
                keep_saved: true,
            })
            .unwrap();
        assert!(tokens.load("fakea").unwrap().is_none());
        assert_eq!(saved(&worker), ["fakea:1", "fakeb:2"]);
    }
}

#[cfg(test)]
mod like_delivery_tests {
    use super::*;
    use eagle_provider_sdk::testkit::{FakeProvider, fake_track};
    use std::sync::atomic::{AtomicI64, Ordering};

    fn id(value: &str) -> TrackId {
        value.parse().unwrap()
    }

    fn catalog_for(providers: Arc<Providers>) -> Catalog {
        let root = std::env::temp_dir().join(format!("eagle-like-catalog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        Catalog {
            covers: RefCell::new(VecDeque::new()),
            last_album: RefCell::new(None),
            last_artist: RefCell::new(None),
            root: root.clone(),
            net: Arc::new(eagle_net::Client::new().unwrap()),
            store: Arc::new(Store::open(&root).unwrap()),
            providers,
            likes_more: std::cell::Cell::new(false),
            clock: wall_clock(),
        }
    }

    fn song(n: u32) -> Track {
        fake_track("fake", &n.to_string(), "Song", "Artist", 1000)
    }

    /// A worker whose only provider is `fake` (signed in) and whose clock the test moves.
    struct Harness {
        worker: Worker,
        tx: mpsc::Sender<Message>,
        rx: mpsc::Receiver<Message>,
        now: Arc<AtomicI64>,
    }

    fn harness(name: &str, fake: &Arc<FakeProvider>) -> Harness {
        let root =
            std::env::temp_dir().join(format!("eagle-like-delivery-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let now = Arc::new(AtomicI64::new(1_000));
        let clock_now = now.clone();
        let clock: Clock = Arc::new(move || clock_now.load(Ordering::SeqCst));
        let (tx, rx) = mpsc::channel();
        let providers: Vec<Arc<dyn Provider>> = vec![fake.clone()];
        let worker = Worker::new_with_providers(
            root,
            Arc::new(|_| {}),
            tx.clone(),
            Some(providers),
            Arc::new(crate::tokens::MemoryTokens::default()),
            clock,
        )
        .unwrap();
        worker
            .providers
            .sign_in(fake.as_ref(), Token::new("t".into()))
            .unwrap();
        Harness {
            worker,
            tx,
            rx,
            now,
        }
    }

    fn wait_until(mut done: impl FnMut() -> bool) {
        let start = Instant::now();
        while !done() {
            assert!(start.elapsed() < Duration::from_secs(5), "timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A signed-in, liked track whose first delivery fails with a network error.
    fn offline_like(name: &str) -> (Harness, Arc<FakeProvider>) {
        let fake = Arc::new(
            FakeProvider::new("fake")
                .with_tracks(vec![song(1)])
                .fail_with(|| ProviderError::Network),
        );
        let h = harness(name, &fake);
        h.worker.store.put_tracks(&[song(1)]).unwrap();
        (h, fake)
    }

    #[test]
    fn like_made_offline_is_delivered_after_the_retry_deadline() {
        let (h, fake) = offline_like("deadline");
        let store = h.worker.store.clone();
        let (tx, now) = (h.tx.clone(), h.now.clone());
        let worker = std::thread::spawn(move || h.worker.run(h.rx).map_err(|e| e.to_string()));
        tx.send(Message::Command(Command::Like(id("fake:1"))))
            .unwrap();
        wait_until(|| store.next_like_retry_at(1_000).unwrap().is_some());
        assert!(fake.set_liked_calls().is_empty());
        // The provider is back and the backoff has passed: any message wakes the worker, which
        // sends the due like without a new command.
        fake.clear_failure();
        now.store(1_000 + 3_600, Ordering::SeqCst);
        tx.send(Message::Engine(
            u64::MAX,
            eagle_engine::Event::StateChanged(State::Idle),
        ))
        .unwrap();
        wait_until(|| !fake.set_liked_calls().is_empty());
        assert_eq!(fake.set_liked_calls(), [(id("fake:1"), true)]);
        tx.send(Message::Command(Command::Shutdown)).unwrap();
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn network_coming_back_flushes_pending_likes() {
        let (h, fake) = offline_like("online");
        let h_store = h.worker.store.clone();
        let (tx, now) = (h.tx.clone(), h.now.clone());
        let worker = std::thread::spawn(move || h.worker.run(h.rx).map_err(|e| e.to_string()));
        tx.send(Message::Command(Command::Like(id("fake:1"))))
            .unwrap();
        wait_until(|| h_store.next_like_retry_at(1_000).unwrap().is_some());
        // The clock has not moved: the network returning alone sends the like.
        fake.clear_failure();
        tx.send(Message::NetworkOnline).unwrap();
        wait_until(|| !fake.set_liked_calls().is_empty());
        assert_eq!(now.load(Ordering::SeqCst), 1_000);
        assert_eq!(fake.set_liked_calls(), [(id("fake:1"), true)]);
        tx.send(Message::Command(Command::Shutdown)).unwrap();
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn search_is_answered_between_like_batches() {
        let tracks: Vec<Track> = (1..=45).map(song).collect();
        let fake = Arc::new(FakeProvider::new("fake").with_tracks(tracks.clone()));
        let providers = Arc::new(Providers::new(vec![fake.clone() as Arc<dyn Provider>], &[]));
        providers.set_account(
            fake.info().id,
            Some(fake.login(Token::new("t".into())).unwrap()),
        );
        let c = catalog_for(providers);
        c.store.put_tracks(&tracks).unwrap();
        for track in &tracks {
            c.store.like(&track.id, true, &["fake".to_owned()]).unwrap();
        }
        let (tx, rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();
        let catalog = std::thread::spawn(move || c.run(rx, reply_tx));
        tx.send(CatalogRequest::FlushLikes { online: false })
            .unwrap();
        tx.send(CatalogRequest::Command(0, Command::Search("x".into())))
            .unwrap();
        // Like the worker: every `LikesPending` queues the next batch behind what arrived since.
        let mut answered = false;
        loop {
            let Message::Catalog(_, reply) = reply_rx.recv_timeout(Duration::from_secs(5)).unwrap()
            else {
                continue;
            };
            match *reply {
                Ok(CatalogReply::LikesPending) => {
                    tx.send(CatalogRequest::FlushLikes { online: false })
                        .unwrap();
                }
                Ok(CatalogReply::Tracks(..)) => {
                    answered = true;
                    // The search came before the third batch was even queued.
                    assert!(fake.set_liked_calls().len() <= 2 * LIKE_BATCH);
                }
                _ => {}
            }
            if answered && fake.set_liked_calls().len() == 45 {
                break;
            }
        }
        drop(tx);
        catalog.join().unwrap();
    }

    #[test]
    fn idle_worker_has_no_like_timer() {
        let fake = Arc::new(FakeProvider::new("fake"));
        let mut h = harness("idle", &fake);
        h.worker.refresh_like_deadline();
        assert_eq!(h.worker.like_wait(), None);
        assert!(!h.worker.like_due());
    }
}
