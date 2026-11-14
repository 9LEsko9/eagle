// SPDX-License-Identifier: GPL-3.0-or-later
use crate::{Capabilities, ProviderInfo, SignInMethod, SignedUrl, Token};
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    str::FromStr,
    time::{Duration, SystemTime},
};

/// Provider-qualified identity; also the SQLite representation.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TrackId {
    provider: String,
    id: String,
}
impl TrackId {
    pub fn provider(&self) -> &str {
        &self.provider
    }
    pub fn local(&self) -> &str {
        &self.id
    }
}
impl fmt::Display for TrackId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.provider, self.id)
    }
}
impl FromStr for TrackId {
    type Err = ProviderError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (provider, id) = value.split_once(':').ok_or(ProviderError::InvalidId)?;
        if provider.is_empty()
            || id.is_empty()
            || value.len() > 200
            || !provider.bytes().all(|c| c.is_ascii_lowercase())
            || !id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
        {
            return Err(ProviderError::InvalidId);
        }
        Ok(Self {
            provider: provider.into(),
            id: id.into(),
        })
    }
}
impl TryFrom<String> for TrackId {
    type Error = ProviderError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}
impl From<TrackId> for String {
    fn from(id: TrackId) -> Self {
        id.to_string()
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artist {
    pub id: TrackId,
    pub name: String,
    /// Image URI template; absent in metadata cached before it was stored.
    #[serde(default)]
    pub cover: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Album {
    pub id: TrackId,
    pub title: String,
    pub artists: Vec<Artist>,
    pub tracks: Vec<Track>,
    #[serde(default)]
    pub cover: Option<String>,
    #[serde(default)]
    pub year: Option<u32>,
}
/// Artist screen: popular tracks and the discography without tracks.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArtistPage {
    pub artist: Artist,
    pub tracks: Vec<Track>,
    pub albums: Vec<Album>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Playlist {
    pub id: TrackId,
    pub title: String,
    pub tracks: Vec<Track>,
    #[serde(default)]
    pub cover: Option<String>,
}
#[derive(Clone, Debug)]
pub struct PlaylistSnapshot {
    pub playlist: Playlist,
    pub revision: u64,
}
#[derive(Clone, Debug)]
pub struct PlaylistTrack {
    pub track_id: TrackId,
    pub album_id: TrackId,
}
#[derive(Clone, Debug)]
pub enum PlaylistEdit {
    Insert {
        at: usize,
        tracks: Vec<PlaylistTrack>,
    },
    Delete {
        from: usize,
        to: usize,
    },
    /// `to` is the final index after removing the source occurrence.
    Move {
        from: usize,
        to: usize,
    },
}
#[derive(Debug, thiserror::Error)]
pub enum PlaylistEditError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("плейлист изменился; обновите список и повторите действие")]
    Conflict,
    #[error("результат изменения неизвестен; обновите список перед повтором")]
    Uncertain,
}
/// How the wave should sound. Providers map it to what their service supports.
///
/// The host passes a tuning only to a provider that declares `radio_tuning` (mood and novelty)
/// or `radio_seed` (`seed`); a provider ignores nothing silently: what it cannot honour it
/// reports as an error before any network request.
#[derive(Clone, Debug, PartialEq)]
pub struct RadioTuning {
    /// Valence (x) and energy (y) in `-1.0..=1.0`; `None` is any mood.
    pub mood: Option<(f32, f32)>,
    /// 0.0 is familiar, 1.0 is new.
    pub novelty: f32,
    /// Start from this track ("more like this").
    pub seed: Option<TrackId>,
}

/// Mood vectors shorter than this (of the unit radius) count as "any mood".
pub const MOOD_DEAD_ZONE: f32 = 0.13;

/// Novelty below this is "familiar", above `NOVELTY_NEW` it is "new".
pub const NOVELTY_FAMILIAR: f32 = 0.36;
/// See `NOVELTY_FAMILIAR`.
pub const NOVELTY_NEW: f32 = 0.64;

/// The eight directions of the mood pad, counter-clockwise from the positive valence axis
/// (valence is x, energy is y): bright, cheerful, energetic, edgy, gloomy, sad, quiet, calm.
pub const MOOD_DIRECTIONS: usize = 8;

/// Which part of the novelty slider a value falls into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoveltyBand {
    Familiar,
    Balanced,
    New,
}

impl Default for RadioTuning {
    /// Any mood, familiar and new in equal measure, no seed.
    fn default() -> Self {
        Self {
            mood: None,
            novelty: 0.5,
            seed: None,
        }
    }
}

impl RadioTuning {
    /// True when mood or novelty ask for more than the plain wave, so `radio_tuning` is needed.
    pub fn is_tuned(&self) -> bool {
        self.normalized().mood_direction().is_some() || self.novelty_band() != NoveltyBand::Balanced
    }

    /// Clamps the mood to the unit disc and the novelty to `0.0..=1.0`; NaN becomes neutral.
    pub fn normalized(&self) -> Self {
        let mood = self.mood.and_then(|(x, y)| {
            if !x.is_finite() || !y.is_finite() {
                return None;
            }
            let length = x.hypot(y);
            let scale = if length > 1.0 { length.recip() } else { 1.0 };
            Some((x * scale, y * scale))
        });
        let novelty = if self.novelty.is_finite() {
            self.novelty.clamp(0.0, 1.0)
        } else {
            0.5
        };
        Self {
            mood,
            novelty,
            seed: self.seed.clone(),
        }
    }

    /// Index `0..MOOD_DIRECTIONS` of the nearest mood direction; `None` for any mood.
    pub fn mood_direction(&self) -> Option<usize> {
        let (x, y) = self.normalized().mood?;
        if x.hypot(y) < MOOD_DEAD_ZONE {
            return None;
        }
        let octant = (y.atan2(x) / std::f32::consts::FRAC_PI_4).round() as i32;
        Some(octant.rem_euclid(MOOD_DIRECTIONS as i32) as usize)
    }

    /// Distance of the mood from the centre, `0.0..=1.0`.
    pub fn mood_strength(&self) -> f32 {
        self.normalized().mood.map_or(0.0, |(x, y)| x.hypot(y))
    }

    pub fn novelty_band(&self) -> NoveltyBand {
        let novelty = self.normalized().novelty;
        if novelty < NOVELTY_FAMILIAR {
            NoveltyBand::Familiar
        } else if novelty > NOVELTY_NEW {
            NoveltyBand::New
        } else {
            NoveltyBand::Balanced
        }
    }
}

#[derive(Clone, Debug)]
pub struct RadioSession {
    pub id: String,
    pub batch: RadioBatch,
}
#[derive(Clone, Debug)]
pub struct RadioBatch {
    pub id: Option<String>,
    pub tracks: Vec<Track>,
}
#[derive(Clone, Debug)]
pub struct RadioQueueItem {
    pub track: TrackId,
    pub album: Option<TrackId>,
}
#[derive(Debug, thiserror::Error)]
pub enum RadioError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("радиосессия завершена; начните новую")]
    UnknownSession,
    #[error("результат запроса радио неизвестен; автоматический повтор отключён")]
    Uncertain,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Availability {
    Available,
    Region,
    Subscription,
    Removed,
    Unknown,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Track {
    pub id: TrackId,
    pub title: String,
    pub artists: Vec<Artist>,
    pub album: Option<TrackId>,
    /// Display title of `album`; absent in metadata cached before it was stored.
    #[serde(default)]
    pub album_title: Option<String>,
    pub duration_ms: u64,
    pub cover: Option<String>,
    pub availability: Availability,
    /// Only set by providers whose `Capabilities::isrc` is true.
    #[serde(default)]
    pub isrc: Option<String>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Quality {
    Standard,
    High,
}
impl Quality {
    pub fn key(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::High => "high",
        }
    }
}
#[derive(Debug)]
pub enum Encryption {
    None,
    Unsupported,
}
#[derive(Debug)]
pub struct Stream {
    /// Bitrate declared by the provider, never inferred from the requested quality.
    pub bitrate_kbps: Option<u32>,
    pub url: SignedUrl,
    pub codec: String,
    pub expires_at: SystemTime,
    pub encryption: Encryption,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub subscription: bool,
    /// Name shown to the user (login or display name); absent when the provider has none.
    #[serde(default)]
    pub name: Option<String>,
    /// What this account may use (subscription, region); narrows `ProviderInfo::capabilities`.
    #[serde(default)]
    pub capabilities: Capabilities,
}
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("результат операции неизвестен; обновите список перед повтором")]
    Uncertain,
    #[error("войдите в аккаунт заново")]
    Auth,
    #[error("ошибка сети; проверьте подключение")]
    Network,
    #[error("не найдено")]
    NotFound,
    #[error("недоступно: регион, подписка или удалённый трек")]
    Unavailable,
    #[error("выбранное качество недоступно для этого трека")]
    UnsupportedQuality,
    #[error("слишком много запросов; повторите позже")]
    RateLimited,
    #[error("формат API изменился")]
    ApiChanged,
    #[error("ожидается provider:id")]
    InvalidId,
    #[error("системное хранилище секретов недоступно")]
    Keyring,
}
/// Opaque in-memory state of a sign-in flow (e.g. a device code). The host hands it back to
/// the provider and never stores, shows or logs it.
#[derive(Clone)]
pub struct SignInFlow(pub Token);

impl fmt::Debug for SignInFlow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SignInFlow([REDACTED])")
    }
}

/// What the user typed for `Provider::sign_in_start`.
pub enum SignInInput {
    /// Methods that need no input (`DeviceCode`, `Browser`).
    None,
    Password {
        login: String,
        password: Token,
    },
}

impl fmt::Debug for SignInInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("None"),
            Self::Password { .. } => f.write_str("Password([REDACTED])"),
        }
    }
}

/// Where a multi-step sign-in stands. The host shows it, waits and calls
/// `Provider::sign_in_continue`; the provider never sleeps or spawns threads itself.
pub enum SignInStep {
    /// Show `code` and ask the user to enter it at `url`, then poll every `interval`
    /// until `expires_in` has passed.
    ShowCode {
        code: String,
        url: String,
        interval: Duration,
        expires_in: Duration,
        flow: SignInFlow,
    },
    /// Open `url` in the system browser, then poll every `interval`.
    OpenBrowser {
        url: String,
        interval: Duration,
        flow: SignInFlow,
    },
    /// Ask the user for a second factor and pass it to `sign_in_continue`; `hint` says where
    /// the code comes from, e.g. "из приложения".
    SecondFactor { hint: String, flow: SignInFlow },
    /// Not finished yet: poll again after the last `interval`.
    Pending,
    /// The code or the flow is no longer valid; start over.
    Expired,
    /// Signed in. The host passes the token to `Provider::login` and stores it.
    Done(Token),
}

impl fmt::Debug for SignInStep {
    // Codes and tokens never reach logs, not even through `{:?}`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ShowCode { .. } => "ShowCode",
            Self::OpenBrowser { .. } => "OpenBrowser",
            Self::SecondFactor { .. } => "SecondFactor",
            Self::Pending => "Pending",
            Self::Expired => "Expired",
            Self::Done(_) => "Done",
        })
    }
}

/// Blocking provider boundary; cursors are opaque to app and engine.
pub struct Page<T> {
    pub items: Vec<T>,
    pub next: Option<String>,
}
/// Only explicit provider evidence changes offline rights.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AccessCheck {
    Unknown,
    Confirmed,
    Revoked,
}
pub trait Provider: Send + Sync {
    /// Starts a wave. Fails without a network request when `tuning` asks for something the
    /// provider does not declare (`radio_tuning`, `radio_seed`).
    fn radio_start(&self, _tuning: &RadioTuning) -> Result<RadioSession, RadioError> {
        Err(ProviderError::Unavailable.into())
    }
    fn radio_next(
        &self,
        _session: &RadioSession,
        _queue: &[RadioQueueItem],
    ) -> Result<RadioBatch, RadioError> {
        Err(ProviderError::Unavailable.into())
    }
    fn check_access(&self, _id: &TrackId) -> Result<AccessCheck, ProviderError> {
        Ok(AccessCheck::Unknown)
    }

    fn info(&self) -> &ProviderInfo;
    fn login(&self, token: Token) -> Result<Account, ProviderError>;
    /// Signs in with a token the host already holds; must not persist it anywhere.
    fn restore(&self, token: Token) -> Result<Account, ProviderError> {
        self.login(token)
    }
    /// Forgets the token held in memory.
    fn logout(&self) {}
    /// Starts a sign-in method from `info().sign_in` other than `Token` (which goes through
    /// `login`). A method the provider does not list returns `Unavailable` without touching
    /// the network.
    fn sign_in_start(
        &self,
        _method: SignInMethod,
        _input: SignInInput,
    ) -> Result<SignInStep, ProviderError> {
        Err(ProviderError::Unavailable)
    }
    /// Polls a code or browser flow (`code: None`) or submits a second factor (`code: Some`).
    /// The host calls it no more often than the flow's `interval`.
    fn sign_in_continue(
        &self,
        _flow: &SignInFlow,
        _code: Option<Token>,
    ) -> Result<SignInStep, ProviderError> {
        Err(ProviderError::Unavailable)
    }
    /// All tracks of an artist, page by page; `cursor` is opaque.
    fn artist_tracks(
        &self,
        _id: &TrackId,
        _cursor: Option<&str>,
    ) -> Result<Page<Track>, ProviderError> {
        Err(ProviderError::Unavailable)
    }
    fn account(&self) -> Result<Account, ProviderError>;
    fn search(&self, query: &str, page: u32) -> Result<Page<Track>, ProviderError>;
    fn search_artists(&self, _query: &str, _page: u32) -> Result<Page<Artist>, ProviderError> {
        Err(ProviderError::Unavailable)
    }
    fn search_albums(&self, _query: &str, _page: u32) -> Result<Page<Album>, ProviderError> {
        Err(ProviderError::Unavailable)
    }
    fn search_playlists(&self, _query: &str, _page: u32) -> Result<Page<Playlist>, ProviderError> {
        Err(ProviderError::Unavailable)
    }
    fn create_playlist(&self, _title: &str) -> Result<Playlist, ProviderError> {
        Err(ProviderError::Unavailable)
    }
    fn delete_playlist(&self, _id: &TrackId) -> Result<(), ProviderError> {
        Err(ProviderError::Unavailable)
    }
    fn playlist_snapshot(&self, _id: &TrackId) -> Result<PlaylistSnapshot, ProviderError> {
        Err(ProviderError::Unavailable)
    }
    fn edit_playlist(
        &self,
        _snapshot: &PlaylistSnapshot,
        _edit: PlaylistEdit,
    ) -> Result<PlaylistSnapshot, PlaylistEditError> {
        Err(ProviderError::Unavailable.into())
    }
    fn track(&self, id: &TrackId) -> Result<Track, ProviderError>;
    fn album(&self, id: &TrackId) -> Result<Album, ProviderError>;
    fn playlist(&self, id: &TrackId) -> Result<Playlist, ProviderError>;
    fn playlists(&self, cursor: Option<&str>) -> Result<Page<Playlist>, ProviderError>;
    fn likes(&self, cursor: Option<&str>) -> Result<Page<Track>, ProviderError>;
    fn set_liked(&self, id: &TrackId, liked: bool) -> Result<(), ProviderError>;
    fn set_disliked(&self, id: &TrackId, disliked: bool) -> Result<(), ProviderError>;
    /// Liked albums without tracks, newest first.
    fn liked_albums(&self) -> Result<Vec<Album>, ProviderError> {
        Ok(Vec::new())
    }
    /// Liked artists, newest first.
    fn liked_artists(&self) -> Result<Vec<Artist>, ProviderError> {
        Ok(Vec::new())
    }
    fn set_album_liked(&self, _id: &TrackId, _liked: bool) -> Result<(), ProviderError> {
        Err(ProviderError::Unavailable)
    }
    fn set_artist_liked(&self, _id: &TrackId, _liked: bool) -> Result<(), ProviderError> {
        Err(ProviderError::Unavailable)
    }
    fn artist(&self, _id: &TrackId) -> Result<ArtistPage, ProviderError> {
        Err(ProviderError::NotFound)
    }
    fn resolve_stream(&self, id: &TrackId, quality: Quality) -> Result<Stream, ProviderError>;
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identities_are_qualified_and_safe_for_paths() {
        let id: TrackId = "yandex:123".parse().unwrap();
        assert_eq!(id.to_string(), "yandex:123");
        for s in ["123", "yandex:", "yandex:../x", "yandex:x:y", ":123"] {
            assert!(s.parse::<TrackId>().is_err());
        }
        assert_eq!("vk:123".parse::<TrackId>().unwrap().provider(), "vk");
    }

    fn mood(x: f32, y: f32) -> RadioTuning {
        RadioTuning {
            mood: Some((x, y)),
            ..RadioTuning::default()
        }
    }

    #[test]
    fn mood_direction_follows_the_pad_order() {
        assert_eq!(mood(0.0, 0.0).mood_direction(), None);
        assert_eq!(mood(0.05, -0.05).mood_direction(), None);
        assert_eq!(RadioTuning::default().mood_direction(), None);
        let expected = [
            ((1.0, 0.0), 0),
            ((0.7, 0.7), 1),
            ((0.0, 1.0), 2),
            ((-0.7, 0.7), 3),
            ((-1.0, 0.0), 4),
            ((-0.7, -0.7), 5),
            ((0.0, -1.0), 6),
            ((0.7, -0.7), 7),
        ];
        for ((x, y), direction) in expected {
            assert_eq!(mood(x, y).mood_direction(), Some(direction), "{x},{y}");
        }
    }

    #[test]
    fn tuning_is_clamped_and_nan_is_neutral() {
        let tuning = RadioTuning {
            mood: Some((3.0, 4.0)),
            novelty: 7.0,
            seed: None,
        }
        .normalized();
        let (x, y) = tuning.mood.unwrap();
        assert!((x.hypot(y) - 1.0).abs() < 1e-5);
        assert_eq!(tuning.novelty, 1.0);
        let broken = RadioTuning {
            mood: Some((f32::NAN, 0.0)),
            novelty: f32::NAN,
            seed: None,
        }
        .normalized();
        assert_eq!(broken.mood, None);
        assert_eq!(broken.novelty, 0.5);
    }

    #[test]
    fn novelty_bands_split_at_the_documented_edges() {
        let band = |novelty| {
            RadioTuning {
                novelty,
                ..RadioTuning::default()
            }
            .novelty_band()
        };
        assert_eq!(band(0.0), NoveltyBand::Familiar);
        assert_eq!(band(0.35), NoveltyBand::Familiar);
        assert_eq!(band(0.5), NoveltyBand::Balanced);
        assert_eq!(band(0.65), NoveltyBand::New);
        assert!(!RadioTuning::default().is_tuned());
        assert!(mood(0.0, 1.0).is_tuned());
    }
}
