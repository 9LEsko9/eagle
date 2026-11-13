// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Self-description of a provider: identity, sign-in method and capabilities.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug)]
pub struct ProviderInfo {
    /// Matches the `TrackId` prefix, e.g. `"yandex"`.
    pub id: &'static str,
    pub name: &'static str,
    pub auth: AuthMethod,
    /// What the provider can do in principle; an account may narrow it.
    pub capabilities: Capabilities,
}

#[derive(Clone, Debug)]
pub enum AuthMethod {
    /// The catalog works without signing in.
    None,
    Token {
        hint: &'static str,
        help_url: Option<&'static str>,
    },
    /// Reserved: the sign-in screen is a separate project.
    Browser {
        start_url: String,
        done: RedirectMatch,
    },
}

/// Final redirect that ends a browser sign-in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RedirectMatch {
    pub url_prefix: String,
}

/// New flags default to `false`, so adding one never breaks an existing provider.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Capabilities {
    pub search_tracks: bool,
    pub search_artists: bool,
    pub search_albums: bool,
    pub search_playlists: bool,
    pub likes: bool,
    pub dislikes: bool,
    pub liked_albums: bool,
    pub liked_artists: bool,
    pub playlists_read: bool,
    pub playlists_edit: bool,
    pub artist_page: bool,
    /// Needed by the artist's "all tracks" screen.
    pub artist_all_tracks: bool,
    pub radio: bool,
    pub play_reporting: bool,
    pub offline: bool,
    pub lyrics: bool,
    /// Whether `Track::isrc` is filled in.
    pub isrc: bool,
}

impl Capabilities {
    /// All flags off; usable in `static`/`const` items, unlike `Default`.
    pub const fn none() -> Self {
        Self {
            search_tracks: false,
            search_artists: false,
            search_albums: false,
            search_playlists: false,
            likes: false,
            dislikes: false,
            liked_albums: false,
            liked_artists: false,
            playlists_read: false,
            playlists_edit: false,
            artist_page: false,
            artist_all_tracks: false,
            radio: false,
            play_reporting: false,
            offline: false,
            lyrics: false,
            isrc: false,
        }
    }
    /// Effective capabilities: what the provider can do and the account allows.
    pub fn intersect(&self, other: &Capabilities) -> Capabilities {
        Capabilities {
            search_tracks: self.search_tracks && other.search_tracks,
            search_artists: self.search_artists && other.search_artists,
            search_albums: self.search_albums && other.search_albums,
            search_playlists: self.search_playlists && other.search_playlists,
            likes: self.likes && other.likes,
            dislikes: self.dislikes && other.dislikes,
            liked_albums: self.liked_albums && other.liked_albums,
            liked_artists: self.liked_artists && other.liked_artists,
            playlists_read: self.playlists_read && other.playlists_read,
            playlists_edit: self.playlists_edit && other.playlists_edit,
            artist_page: self.artist_page && other.artist_page,
            artist_all_tracks: self.artist_all_tracks && other.artist_all_tracks,
            radio: self.radio && other.radio,
            play_reporting: self.play_reporting && other.play_reporting,
            offline: self.offline && other.offline,
            lyrics: self.lyrics && other.lyrics,
            isrc: self.isrc && other.isrc,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_has_nothing_enabled() {
        let none = Capabilities::default();
        assert!(!none.likes && !none.search_tracks && !none.isrc);
        assert_eq!(none.intersect(&none), none);
    }

    #[test]
    fn intersect_is_per_flag() {
        let provider = Capabilities {
            likes: true,
            radio: true,
            ..Capabilities::default()
        };
        let account = Capabilities {
            likes: true,
            lyrics: true,
            ..Capabilities::default()
        };
        let effective = provider.intersect(&account);
        assert!(effective.likes);
        assert!(!effective.radio && !effective.lyrics);
    }

    #[test]
    fn missing_or_partial_json_fills_defaults() {
        let parsed: Capabilities = serde_json::from_str(r#"{"likes":true}"#).unwrap();
        assert!(parsed.likes && !parsed.radio);
    }
}
