// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Minimal provider to copy when starting a new one.
//!
//! Replace the in-memory catalog with calls to the service, keep the method shapes,
//! and change `info()` to describe what the service really supports.
//! The rules are in `crates/provider-sdk/README.md`.

use eagle_provider_sdk::*;
use std::time::{Duration, SystemTime};

/// Provider id; it must match `[a-z]+` because it is the `TrackId` prefix.
// Replace: the new provider's id, for example "acme".
const ID: &str = "template";

pub struct TemplateProvider {
    info: ProviderInfo,
}

impl TemplateProvider {
    pub fn new() -> Self {
        // Replace: declare only the capabilities the service really provides.
        let mut capabilities = Capabilities::none();
        capabilities.search_tracks = true;
        capabilities.artist_page = true;
        Self {
            info: ProviderInfo {
                id: ID,
                // Replace: the name shown to users.
                name: "Template",
                // This template has no sign-in. List `SignInMethod`s, preferred first, for a
                // service with accounts: `Token { .. }` goes through `login`; `DeviceCode`,
                // `Password` and `Browser` go through `sign_in_start` and `sign_in_continue`.
                sign_in: &[],
                capabilities,
            },
        }
    }
}

impl Default for TemplateProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// Builds an id in this provider's namespace.
fn track_id(local: &str) -> Result<TrackId, ProviderError> {
    format!("{ID}:{local}").parse()
}

// Replace: the in-memory catalog with requests to the service.
fn catalog() -> Result<Vec<Track>, ProviderError> {
    let artist = Artist {
        id: track_id("artist-example")?,
        name: "Example Artist".into(),
        cover: None,
    };
    let track = |local: &str, title: &str, duration_ms: u64| -> Result<Track, ProviderError> {
        Ok(Track {
            id: track_id(local)?,
            title: title.into(),
            artists: vec![artist.clone()],
            album: None,
            album_title: None,
            duration_ms,
            cover: None,
            availability: Availability::Available,
            isrc: None,
        })
    };
    Ok(vec![
        track("1", "First Song", 180_000)?,
        track("2", "Second Song", 200_000)?,
    ])
}

fn page<T>(items: Vec<T>) -> Page<T> {
    Page { items, next: None }
}

impl Provider for TemplateProvider {
    fn info(&self) -> &ProviderInfo {
        &self.info
    }

    // Replace: exchange the token with the service and return the account it belongs to.
    // Do not store the token here; the host keeps it.
    fn login(&self, _token: Token) -> Result<Account, ProviderError> {
        self.account()
    }

    fn account(&self) -> Result<Account, ProviderError> {
        Ok(Account {
            id: "template-user".into(),
            name: None,
            subscription: true,
            capabilities: self.info.capabilities,
        })
    }

    // Replace: a request to the service's search endpoint. Keep `page` numeric.
    fn search(&self, query: &str, page_index: u32) -> Result<Page<Track>, ProviderError> {
        if page_index > 0 {
            return Ok(page(Vec::new()));
        }
        let needle = query.to_lowercase();
        let tracks = catalog()?
            .into_iter()
            .filter(|t| t.title.to_lowercase().contains(&needle))
            .collect();
        Ok(page(tracks))
    }

    fn artist(&self, id: &TrackId) -> Result<ArtistPage, ProviderError> {
        let tracks: Vec<Track> = catalog()?
            .into_iter()
            .filter(|t| t.artists.iter().any(|a| a.id == *id))
            .collect();
        let artist = tracks
            .first()
            .and_then(|t| t.artists.iter().find(|a| a.id == *id).cloned())
            .ok_or(ProviderError::NotFound)?;
        Ok(ArtistPage {
            artist,
            tracks,
            albums: Vec::new(),
        })
    }

    fn track(&self, id: &TrackId) -> Result<Track, ProviderError> {
        catalog()?
            .into_iter()
            .find(|t| t.id == *id)
            .ok_or(ProviderError::NotFound)
    }

    fn album(&self, _id: &TrackId) -> Result<Album, ProviderError> {
        Err(ProviderError::NotFound)
    }

    fn playlist(&self, _id: &TrackId) -> Result<Playlist, ProviderError> {
        Err(ProviderError::NotFound)
    }

    fn playlists(&self, _cursor: Option<&str>) -> Result<Page<Playlist>, ProviderError> {
        Ok(page(Vec::new()))
    }

    fn likes(&self, _cursor: Option<&str>) -> Result<Page<Track>, ProviderError> {
        Ok(page(Vec::new()))
    }

    fn set_liked(&self, _id: &TrackId, _liked: bool) -> Result<(), ProviderError> {
        Err(ProviderError::Unavailable)
    }

    fn set_disliked(&self, _id: &TrackId, _disliked: bool) -> Result<(), ProviderError> {
        Err(ProviderError::Unavailable)
    }

    // Replace: ask the service for a link. The link must expire; never log it.
    fn resolve_stream(&self, id: &TrackId, _quality: Quality) -> Result<Stream, ProviderError> {
        self.track(id)?;
        Ok(Stream {
            bitrate_kbps: Some(128),
            url: SignedUrl::new(format!("https://example.invalid/{}", id.local())),
            codec: "mp3".into(),
            expires_at: SystemTime::now() + Duration::from_secs(3600),
            encryption: Encryption::None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eagle_provider_sdk::testkit::conformance::{self, Fixtures};

    #[test]
    fn passes_conformance() {
        let provider = TemplateProvider::new();
        let fixtures = Fixtures {
            known_track: track_id("1").unwrap(),
            query: "song".into(),
            artist: Some(track_id("artist-example").unwrap()),
            signed_in: true,
        };
        let violations = conformance::run(&provider, &fixtures);
        assert!(violations.is_empty(), "{violations:?}");
    }
}
