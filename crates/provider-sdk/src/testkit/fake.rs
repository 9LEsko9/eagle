// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! In-memory provider with a configurable catalog, sign-in state and failure injection.

use crate::AuthMethod;
use crate::{
    Account, Album, Artist, ArtistPage, Availability, Capabilities, Encryption, Page, Playlist,
    PlaylistSnapshot, Provider, ProviderError, ProviderInfo, Quality, RadioBatch, RadioError,
    RadioSession, SignedUrl, Stream, Token, Track, TrackId,
};
use std::{
    sync::{Mutex, MutexGuard, PoisonError},
    thread,
    time::{Duration, SystemTime},
};

/// Items per page; `next` is the index of the following page as a string.
const PAGE_SIZE: usize = 20;

type Failure = Box<dyn Fn() -> ProviderError + Send + Sync>;

/// Configurable provider for host and provider tests.
///
/// Every network-like method checks, in order, the injected failure and the sign-in state.
pub struct FakeProvider {
    info: ProviderInfo,
    tracks: Vec<Track>,
    unavailable: Vec<&'static str>,
    failure: Option<Failure>,
    delay: Duration,
    state: Mutex<State>,
}

struct State {
    signed_in: bool,
    failing: bool,
    /// Newest first.
    liked: Vec<TrackId>,
    resolve_calls: Vec<TrackId>,
    set_liked_calls: Vec<(TrackId, bool)>,
    search_calls: usize,
}

impl FakeProvider {
    /// Creates a signed-in provider with an empty catalog and most capabilities enabled.
    pub fn new(id: &'static str) -> Self {
        let mut capabilities = Capabilities::none();
        capabilities.search_tracks = true;
        capabilities.search_artists = true;
        capabilities.search_albums = true;
        capabilities.search_playlists = true;
        capabilities.likes = true;
        capabilities.dislikes = true;
        capabilities.liked_albums = true;
        capabilities.liked_artists = true;
        capabilities.playlists_read = true;
        capabilities.artist_page = true;
        capabilities.radio = true;
        capabilities.play_reporting = true;
        capabilities.offline = true;
        Self {
            info: ProviderInfo {
                id,
                name: id,
                auth: AuthMethod::Token {
                    hint: "fake token",
                    help_url: None,
                },
                capabilities,
            },
            tracks: Vec::new(),
            unavailable: Vec::new(),
            failure: None,
            delay: Duration::ZERO,
            state: Mutex::new(State {
                signed_in: true,
                failing: false,
                liked: Vec::new(),
                resolve_calls: Vec::new(),
                set_liked_calls: Vec::new(),
                search_calls: 0,
            }),
        }
    }

    /// Replaces the catalog.
    pub fn with_tracks(mut self, tracks: Vec<Track>) -> Self {
        self.tracks = tracks;
        self
    }

    /// Replaces the declared capabilities.
    pub fn with_capabilities(mut self, capabilities: Capabilities) -> Self {
        self.info.capabilities = capabilities;
        self
    }

    /// Starts signed out; `login` with a non-empty token signs in.
    pub fn signed_out(mut self) -> Self {
        self.state_mut().signed_in = false;
        self
    }

    /// Local ids for which `resolve_stream` returns `Unavailable`.
    pub fn with_unavailable(mut self, local_ids: &[&'static str]) -> Self {
        self.unavailable.extend_from_slice(local_ids);
        self
    }

    /// Makes every catalog and stream method return `failure()` until `clear_failure`.
    pub fn fail_with(
        mut self,
        failure: impl Fn() -> ProviderError + Send + Sync + 'static,
    ) -> Self {
        self.failure = Some(Box::new(failure));
        self.state_mut().failing = true;
        self
    }

    /// Stops the failure injected by `fail_with`.
    pub fn clear_failure(&self) {
        self.lock().failing = false;
    }

    /// Sleeps in `search` for `delay`, to imitate a slow provider.
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// Track ids passed to `resolve_stream`, in call order.
    pub fn resolve_calls(&self) -> Vec<TrackId> {
        self.lock().resolve_calls.clone()
    }

    /// Currently liked tracks, newest first.
    pub fn liked_ids(&self) -> Vec<TrackId> {
        self.lock().liked.clone()
    }

    /// Arguments of `set_liked` calls, in call order.
    pub fn set_liked_calls(&self) -> Vec<(TrackId, bool)> {
        self.lock().set_liked_calls.clone()
    }

    /// Number of `search` calls, including failed ones.
    pub fn search_calls(&self) -> usize {
        self.lock().search_calls
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn state_mut(&mut self) -> &mut State {
        self.state.get_mut().unwrap_or_else(PoisonError::into_inner)
    }

    /// Applies the injected failure first, then the sign-in check.
    fn guard(&self) -> Result<(), ProviderError> {
        let state = self.lock();
        if state.failing {
            return Err(self
                .failure
                .as_ref()
                .map_or(ProviderError::Network, |f| f()));
        }
        if state.signed_in {
            Ok(())
        } else {
            Err(ProviderError::Auth)
        }
    }

    fn account_value(&self) -> Account {
        Account {
            id: "fake-uid".into(),
            subscription: true,
            capabilities: self.info.capabilities,
        }
    }

    fn track_by_id(&self, id: &TrackId) -> Option<&Track> {
        self.tracks.iter().find(|t| t.id == *id)
    }

    fn artist_tracks_of(&self, id: &TrackId) -> Vec<Track> {
        self.tracks
            .iter()
            .filter(|t| t.artists.iter().any(|a| a.id == *id))
            .cloned()
            .collect()
    }

    fn find_artist(&self, id: &TrackId) -> Option<Artist> {
        self.tracks
            .iter()
            .flat_map(|t| t.artists.iter())
            .find(|a| a.id == *id)
            .cloned()
    }

    /// Distinct artists in catalog order.
    fn all_artists(&self) -> Vec<Artist> {
        let mut seen: Vec<TrackId> = Vec::new();
        let mut artists = Vec::new();
        for artist in self.tracks.iter().flat_map(|t| t.artists.iter()) {
            if !seen.contains(&artist.id) {
                seen.push(artist.id.clone());
                artists.push(artist.clone());
            }
        }
        artists
    }

    fn build_album(&self, id: &TrackId) -> Option<Album> {
        let tracks: Vec<Track> = self
            .tracks
            .iter()
            .filter(|t| t.album.as_ref() == Some(id))
            .cloned()
            .collect();
        let first = tracks.first()?;
        Some(Album {
            id: id.clone(),
            title: first
                .album_title
                .clone()
                .unwrap_or_else(|| id.local().into()),
            artists: first.artists.clone(),
            cover: first.cover.clone(),
            year: None,
            tracks,
        })
    }

    /// Distinct album ids in catalog order.
    fn album_ids(&self) -> Vec<TrackId> {
        let mut ids: Vec<TrackId> = Vec::new();
        for id in self.tracks.iter().filter_map(|t| t.album.as_ref()) {
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
        ids
    }
}

fn page_of<T>(items: Vec<T>, index: usize) -> Page<T> {
    let start = index.saturating_mul(PAGE_SIZE);
    let next = (start.saturating_add(PAGE_SIZE) < items.len()).then(|| (index + 1).to_string());
    Page {
        items: items.into_iter().skip(start).take(PAGE_SIZE).collect(),
        next,
    }
}

/// Parses a cursor produced by `page_of`; `None` is the first page.
fn cursor_index(cursor: Option<&str>) -> Result<usize, ProviderError> {
    match cursor {
        None => Ok(0),
        Some(value) => value.parse().map_err(|_| ProviderError::NotFound),
    }
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

/// Builds a track whose artist id is `<provider>:artist-<slug>`.
///
/// # Panics
///
/// Panics if `provider` or `local` does not form a valid `TrackId`, which is a test setup error.
pub fn fake_track(
    provider: &str,
    local: &str,
    title: &str,
    artist: &str,
    duration_ms: u64,
) -> Track {
    let slug: String = artist
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let id = parse_id(&format!("{provider}:{local}"));
    let artist_id = parse_id(&format!("{provider}:artist-{slug}"));
    Track {
        id,
        title: title.into(),
        artists: vec![Artist {
            id: artist_id,
            name: artist.into(),
            cover: None,
        }],
        album: None,
        album_title: None,
        duration_ms,
        cover: None,
        availability: Availability::Available,
        isrc: None,
    }
}

fn parse_id(value: &str) -> TrackId {
    match value.parse() {
        Ok(id) => id,
        Err(err) => panic!("invalid fake id {value:?}: {err}"),
    }
}

impl Provider for FakeProvider {
    fn radio_start(&self) -> Result<RadioSession, RadioError> {
        self.guard()?;
        Ok(RadioSession {
            id: "fake-session".into(),
            batch: RadioBatch {
                id: None,
                tracks: self.tracks.clone(),
            },
        })
    }

    fn radio_next(
        &self,
        _session: &RadioSession,
        _queue: &[crate::RadioQueueItem],
    ) -> Result<RadioBatch, RadioError> {
        self.guard()?;
        Ok(RadioBatch {
            id: None,
            tracks: Vec::new(),
        })
    }

    fn info(&self) -> &ProviderInfo {
        &self.info
    }

    fn login(&self, token: Token) -> Result<Account, ProviderError> {
        if token.expose().is_empty() {
            return Err(ProviderError::Auth);
        }
        self.lock().signed_in = true;
        Ok(self.account_value())
    }

    fn logout(&self) {
        self.lock().signed_in = false;
    }

    fn account(&self) -> Result<Account, ProviderError> {
        self.guard()?;
        Ok(self.account_value())
    }

    fn artist_tracks(
        &self,
        id: &TrackId,
        cursor: Option<&str>,
    ) -> Result<Page<Track>, ProviderError> {
        self.guard()?;
        let index = cursor_index(cursor)?;
        let tracks = self.artist_tracks_of(id);
        if tracks.is_empty() {
            return Err(ProviderError::NotFound);
        }
        Ok(page_of(tracks, index))
    }

    fn search(&self, query: &str, page: u32) -> Result<Page<Track>, ProviderError> {
        self.lock().search_calls += 1;
        self.guard()?;
        thread::sleep(self.delay);
        let matches = self
            .tracks
            .iter()
            .filter(|t| contains_ci(&t.title, query))
            .cloned()
            .collect();
        Ok(page_of(matches, page as usize))
    }

    fn search_artists(&self, query: &str, page: u32) -> Result<Page<Artist>, ProviderError> {
        self.guard()?;
        let matches = self
            .all_artists()
            .into_iter()
            .filter(|a| contains_ci(&a.name, query))
            .collect();
        Ok(page_of(matches, page as usize))
    }

    fn search_albums(&self, query: &str, page: u32) -> Result<Page<Album>, ProviderError> {
        self.guard()?;
        let matches = self
            .album_ids()
            .iter()
            .filter_map(|id| self.build_album(id))
            .filter(|a| contains_ci(&a.title, query))
            .collect();
        Ok(page_of(matches, page as usize))
    }

    fn search_playlists(&self, _query: &str, page: u32) -> Result<Page<Playlist>, ProviderError> {
        self.guard()?;
        Ok(page_of(Vec::new(), page as usize))
    }

    fn playlist_snapshot(&self, _id: &TrackId) -> Result<PlaylistSnapshot, ProviderError> {
        self.guard()?;
        Err(ProviderError::NotFound)
    }

    fn track(&self, id: &TrackId) -> Result<Track, ProviderError> {
        self.guard()?;
        self.track_by_id(id).cloned().ok_or(ProviderError::NotFound)
    }

    fn album(&self, id: &TrackId) -> Result<Album, ProviderError> {
        self.guard()?;
        self.build_album(id).ok_or(ProviderError::NotFound)
    }

    fn playlist(&self, _id: &TrackId) -> Result<Playlist, ProviderError> {
        self.guard()?;
        Err(ProviderError::NotFound)
    }

    fn playlists(&self, cursor: Option<&str>) -> Result<Page<Playlist>, ProviderError> {
        self.guard()?;
        Ok(page_of(Vec::new(), cursor_index(cursor)?))
    }

    fn likes(&self, cursor: Option<&str>) -> Result<Page<Track>, ProviderError> {
        self.guard()?;
        let index = cursor_index(cursor)?;
        let liked = self
            .lock()
            .liked
            .iter()
            .filter_map(|id| self.track_by_id(id).cloned())
            .collect();
        Ok(page_of(liked, index))
    }

    fn set_liked(&self, id: &TrackId, liked: bool) -> Result<(), ProviderError> {
        self.guard()?;
        if self.track_by_id(id).is_none() {
            return Err(ProviderError::NotFound);
        }
        let mut state = self.lock();
        state.set_liked_calls.push((id.clone(), liked));
        state.liked.retain(|known| known != id);
        if liked {
            state.liked.insert(0, id.clone());
        }
        Ok(())
    }

    fn set_disliked(&self, _id: &TrackId, _disliked: bool) -> Result<(), ProviderError> {
        self.guard()
    }

    fn artist(&self, id: &TrackId) -> Result<ArtistPage, ProviderError> {
        self.guard()?;
        let artist = self.find_artist(id).ok_or(ProviderError::NotFound)?;
        Ok(ArtistPage {
            artist,
            tracks: self.artist_tracks_of(id),
            albums: Vec::new(),
        })
    }

    fn resolve_stream(&self, id: &TrackId, _quality: Quality) -> Result<Stream, ProviderError> {
        self.lock().resolve_calls.push(id.clone());
        self.guard()?;
        if self.unavailable.iter().any(|local| *local == id.local()) {
            return Err(ProviderError::Unavailable);
        }
        if self.track_by_id(id).is_none() {
            return Err(ProviderError::NotFound);
        }
        Ok(Stream {
            bitrate_kbps: Some(128),
            url: SignedUrl::new(format!("https://fake.invalid/{id}")),
            codec: "mp3".into(),
            expires_at: SystemTime::now() + Duration::from_secs(3600),
            encryption: Encryption::None,
        })
    }
}
