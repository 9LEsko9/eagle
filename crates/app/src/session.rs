// SPDX-License-Identifier: GPL-3.0-or-later
//! Desktop-facing commands and incremental events. No UI types cross this boundary.
pub use eagle_core::{Availability, Token};
use eagle_core::{Provider, Quality, Track, TrackId};
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Library,
    Search,
    Album,
    Playlist,
    Login,
    Settings,
}

pub enum Command {
    Open(View),
    Library,
    SyncLibrary,
    Collections,
    Search(String),
    Login(Token),
    Album(TrackId),
    Playlist(TrackId),
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Collection {
    pub id: TrackId,
    pub title: String,
}
pub enum Event {
    Collections(Vec<Collection>),
    Collection { title: String, length: Duration },
    Queue(Vec<Arc<Track>>),
    View(View),
    Busy(bool),
    Player(Arc<PlayerState>),
    Position(PositionAnchor),
    Library(Vec<LibraryDelta>),
    Error(String),
    Account,
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
    queue_base: usize,
    heard_audio: bool,
    view_generation: u64,
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
            root: root.clone(),
            net: net.clone(),
            store: store.clone(),
            provider: provider.clone(),
        };
        let replies = tx.clone();
        let catalog_worker = thread::Builder::new()
            .name("eg-catalog".into())
            .spawn(move || {
                for request in requests {
                    let generation = match &request {
                        CatalogRequest::Command(generation, _) => *generation,
                        CatalogRequest::Cover(_, _, generation) => *generation,
                    };
                    let reply = services.request(request).map_err(|e| e.to_string());
                    if replies.send(Message::Catalog(generation, reply)).is_err() {
                        break;
                    }
                }
            })?;
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
            queue_base: 0,
            heard_audio: false,
            view_generation: 0,
            last_position: None,
            player: Arc::default(),
            emit,
        })
    }
    fn run(&mut self, rx: mpsc::Receiver<Message>) -> Result<()> {
        (self.emit)(Event::Player(self.player.clone()));
        self.request(Command::Collections)?;
        self.request(Command::Library)?;
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
                    if generation != 0 && generation != self.view_generation {
                        continue;
                    }
                    if generation != 0 {
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
                        Ok(CatalogReply::Liked(id, liked)) => {
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
                        Ok(CatalogReply::Collections(collections)) => {
                            (self.emit)(Event::Collections(collections))
                        }
                        Ok(CatalogReply::Unchanged) => {}
                        Ok(CatalogReply::Cover(target, id, cover)) => {
                            (self.emit)(Event::Cover(target, id, cover))
                        }
                        Ok(CatalogReply::Account) => {
                            (self.emit)(Event::Account);
                            self.request(Command::SyncLibrary)?;
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
    fn stop_engine(&mut self) {
        self.engine_generation = self.engine_generation.wrapping_add(1);
        self.engine.take();
        if let Some(worker) = self.engine_events.take() {
            let _ = worker.join();
        }
    }
    fn publish(&mut self, incoming: Vec<LibraryItem>) {
        let delta = reconcile(&mut self.rows, incoming);
        if !delta.is_empty() {
            (self.emit)(Event::Library(delta));
        }
    }
    fn request(&mut self, command: Command) -> Result<()> {
        let generation = if matches!(
            &command,
            Command::Library
                | Command::SyncLibrary
                | Command::Collections
                | Command::Search(_)
                | Command::Album(_)
                | Command::Playlist(_)
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
                (self.emit)(Event::View(view));
                self.view_generation += 1;
                (self.emit)(Event::Busy(false));
                if view == View::Library {
                    self.request(Command::Library)?;
                } else if matches!(view, View::Search | View::Album | View::Playlist) {
                    self.publish(Vec::new());
                }
            }
            command @ (Command::Library
            | Command::SyncLibrary
            | Command::Collections
            | Command::Search(_)
            | Command::Login(_)
            | Command::Like(_)) => self.request(command)?,
            Command::Album(id) => {
                (self.emit)(Event::View(View::Album));
                self.publish(Vec::new());
                self.request(Command::Album(id))?;
            }
            Command::Playlist(id) => {
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
                self.queue = self
                    .rows
                    .iter()
                    .filter(|r| r.track.availability == Availability::Available)
                    .map(|r| r.track.clone())
                    .collect();
                self.queue_original = self.queue.clone();
                let track = self.rows[index].track.clone();
                self.queue_likes = self
                    .rows
                    .iter()
                    .filter(|r| r.liked)
                    .map(|r| r.track.id.clone())
                    .collect();
                if self.player.shuffle {
                    use std::hash::{BuildHasher, RandomState};
                    self.queue.retain(|t| t.id != track.id);
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
                if self.player.repeat == Repeat::One && self.current + 1 < self.queue.len() {
                    let paused = self.player.status == State::Paused;
                    self.current += 1;
                    self.play_current()?;
                    if paused && let Some(engine) = &self.engine {
                        engine.pause()?;
                    }
                } else if let Some(engine) = &self.engine {
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
                        .map(|(i, t)| (t.id.clone(), i))
                        .collect();
                    self.queue[self.current + 1..]
                        .sort_by_key(|t| order.get(&t.id).copied().unwrap_or(usize::MAX));
                }
                self.rebuild()?;
                (self.emit)(Event::Player(self.player.clone()));
            }
            Command::CycleRepeat => {
                let state = Arc::make_mut(&mut self.player);
                state.repeat = match state.repeat {
                    Repeat::Off => Repeat::All,
                    Repeat::All => Repeat::One,
                    Repeat::One => Repeat::Off,
                };
                self.rebuild()?;
                (self.emit)(Event::Player(self.player.clone()));
            }
            Command::Shutdown => {}
        }
        Ok(())
    }
    fn rebuild(&mut self) -> Result<()> {
        if self.engine.is_none() {
            return Ok(());
        }
        let paused = self.player.status == State::Paused;
        let position = self
            .last_position
            .as_ref()
            .map_or(Duration::ZERO, |a| position_now(a, Instant::now()));
        self.play_current()?;
        if let Some(engine) = &self.engine {
            engine.seek(position)?;
            if paused {
                engine.pause()?;
            }
        }
        Ok(())
    }
    fn emit_queue(&self) {
        (self.emit)(Event::Queue(
            self.queue
                .iter()
                .skip(self.current + 1)
                .take(4)
                .cloned()
                .collect(),
        ));
    }
    fn play_current(&mut self) -> Result<()> {
        let Some(track) = self.queue.get(self.current).cloned() else {
            return Ok(());
        };
        log::info!(target: "eagle_app", provider = track.id.provider(), track_id = track.id.local(); "playback");
        self.stop_engine();
        let mut engine = Engine::new(Config::default())?;
        engine.volume(self.player.volume)?;
        self.queue_base = self.current;
        self.heard_audio = false;
        let end = if self.player.repeat == Repeat::One {
            self.current + 1
        } else {
            self.queue.len()
        };
        for item in &self.queue[self.current..end] {
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
                self.current = self.queue_base + index as usize;
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
                if status == State::Playing {
                    self.heard_audio = true;
                }
                if status == State::Idle && self.heard_audio && self.player.repeat != Repeat::Off {
                    if self.player.repeat == Repeat::All {
                        self.current = 0;
                    }
                    if let Err(error) = self.play_current() {
                        (self.emit)(Event::Error(error.to_string()));
                    }
                    return;
                }
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

enum CatalogRequest {
    Command(u64, Command),
    Cover(Arc<Track>, CoverTarget, u64),
}
enum CatalogReply {
    Collections(Vec<Collection>),
    Unchanged,
    Cover(CoverTarget, TrackId, Arc<eagle_palette::Cover>),
    Tracks(Vec<LibraryItem>, String),
    Liked(TrackId, bool),
    Account,
}
struct Catalog {
    covers: RefCell<VecDeque<(TrackId, Arc<eagle_palette::Cover>)>>,
    root: PathBuf,
    net: Arc<eagle_net::Client>,
    store: Arc<Store>,
    provider: Arc<dyn Provider>,
}
impl Catalog {
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
            let mut stmt = c.prepare("SELECT tracks.json, EXISTS(SELECT 1 FROM likes WHERE likes.id=tracks.id) FROM tracks ORDER BY tracks.id")?;
            stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?)))?
                .map(|row| {
                    let (json, liked) = row?;
                    Ok(LibraryItem {
                        track: Arc::new(serde_json::from_str(&json)?),
                        liked,
                    })
                })
                .collect::<Result<Vec<_>>>()?
        };
        Ok(CatalogReply::Tracks(rows, String::new()))
    }
    fn tracks(&self, tracks: Vec<Track>, title: String) -> Result<CatalogReply> {
        let mut items = Vec::with_capacity(tracks.len());
        for track in tracks {
            self.store.put_track(&track)?;
            let liked = self.store.read()?.query_row(
                "SELECT EXISTS(SELECT 1 FROM likes WHERE id=?1)",
                [track.id.to_string()],
                |r| r.get(0),
            )?;
            items.push(LibraryItem {
                track: Arc::new(track),
                liked,
            });
        }
        Ok(CatalogReply::Tracks(items, title))
    }
    fn command(&self, command: Command) -> Result<CatalogReply> {
        match command {
            Command::Collections => {
                let c = self.store.read()?;
                let mut stmt = c.prepare("SELECT json FROM playlists ORDER BY id")?;
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
            Command::SyncLibrary => {
                crate::music::sync(&self.store, self.provider.as_ref())?;
                self.library()
            }
            Command::Search(query) => {
                self.tracks(self.provider.search(&query, 0)?.items, String::new())
            }
            Command::Album(id) => {
                let a = self.provider.album(&id)?;
                self.tracks(a.tracks, a.title)
            }
            Command::Playlist(id) => {
                let p = self.provider.playlist(&id)?;
                self.tracks(p.tracks, p.title)
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
                self.provider.set_liked(&id, liked)?;
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
                duration_ms: 1000,
                cover: None,
                availability: Availability::Available,
            }),
            liked,
        }
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
                    duration_ms: 1000,
                    cover: None,
                    availability: Availability::Available,
                })
                .unwrap();
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
