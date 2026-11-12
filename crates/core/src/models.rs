// SPDX-License-Identifier: GPL-3.0-or-later
use crate::{SignedUrl, Token};
use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr, time::SystemTime};

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
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Album {
    pub id: TrackId,
    pub title: String,
    pub artists: Vec<Artist>,
    pub tracks: Vec<Track>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Playlist {
    pub id: TrackId,
    pub title: String,
    pub tracks: Vec<Track>,
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
    pub duration_ms: u64,
    pub cover: Option<String>,
    pub availability: Availability,
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
    pub url: SignedUrl,
    pub codec: String,
    pub expires_at: SystemTime,
    pub encryption: Encryption,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub subscription: bool,
}
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("войдите в аккаунт заново")]
    Auth,
    #[error("ошибка сети; проверьте подключение")]
    Network,
    #[error("не найдено")]
    NotFound,
    #[error("недоступно: регион, подписка или удалённый трек")]
    Unavailable,
    #[error("слишком много запросов; повторите позже")]
    RateLimited,
    #[error("формат API изменился")]
    ApiChanged,
    #[error("ожидается provider:id")]
    InvalidId,
    #[error("системное хранилище секретов недоступно")]
    Keyring,
}
/// Blocking provider boundary; cursors are opaque to app and engine.
pub struct Page<T> {
    pub items: Vec<T>,
    pub next: Option<String>,
}
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    fn login(&self, token: Token) -> Result<Account, ProviderError>;
    fn account(&self) -> Result<Account, ProviderError>;
    fn search(&self, query: &str, page: u32) -> Result<Page<Track>, ProviderError>;
    fn track(&self, id: &TrackId) -> Result<Track, ProviderError>;
    fn album(&self, id: &TrackId) -> Result<Album, ProviderError>;
    fn playlist(&self, id: &TrackId) -> Result<Playlist, ProviderError>;
    fn playlists(&self, cursor: Option<&str>) -> Result<Page<Playlist>, ProviderError>;
    fn likes(&self, cursor: Option<&str>) -> Result<Page<Track>, ProviderError>;
    fn set_liked(&self, id: &TrackId, liked: bool) -> Result<(), ProviderError>;
    fn set_disliked(&self, id: &TrackId, disliked: bool) -> Result<(), ProviderError>;
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
}
