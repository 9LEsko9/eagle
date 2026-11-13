// SPDX-License-Identifier: GPL-3.0-or-later
//! Proposed provider contracts. Wire compatibility requires live verification.

pub use eagle_core::{PlaylistEdit, PlaylistEditError, PlaylistSnapshot, PlaylistTrack};
use eagle_core::{Track, TrackId};

/// Identifiers are opaque and must remain associated with their originating session.
#[derive(Clone, Debug)]
pub struct RadioSession {
    pub session_id: String,
    pub batch: RadioBatch,
}

#[derive(Clone, Debug)]
pub struct RadioBatch {
    pub batch_id: Option<String>,
    pub tracks: Vec<Track>,
}

#[derive(Clone, Debug)]
pub enum RadioEventKind {
    RadioStarted,
    TrackStarted,
    TrackFinished,
    Skip,
}

#[derive(Clone, Debug)]
pub struct RadioEvent {
    pub session_id: String,
    pub batch_id: Option<String>,
    pub kind: RadioEventKind,
    pub track_id: Option<TrackId>,
    pub played_ms: Option<u64>,
}

/// Stable locally assigned identity does not imply server-side deduplication.
#[derive(Clone, Debug)]
pub struct PlaybackEvent {
    pub id: String,
    pub track_id: TrackId,
    pub played_ms: u64,
    pub occurred_at_unix_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AccessCheck {
    Unknown,
    Confirmed,
    Revoked { reason: String },
}

impl super::Yandex {
    /// Experimental: the returned revision is required for a positional edit.
    pub fn playlist_snapshot_unverified(
        &self,
        id: &TrackId,
    ) -> Result<PlaylistSnapshot, eagle_core::ProviderError> {
        let (owner, kind) = self
            .local(id)?
            .split_once('_')
            .ok_or(eagle_core::ProviderError::InvalidId)?;
        if owner.is_empty()
            || kind.is_empty()
            || !owner.bytes().all(|b| b.is_ascii_digit())
            || !kind.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(eagle_core::ProviderError::InvalidId);
        }
        let value = self.api(&format!("/users/{owner}/playlists/{kind}"), None)?;
        let count = match value.get("trackCount") {
            Some(count) => Some(
                count
                    .as_u64()
                    .ok_or(eagle_core::ProviderError::ApiChanged)?,
            ),
            None => None,
        };
        match value.get("tracks") {
            Some(tracks) => {
                let tracks = tracks
                    .as_array()
                    .ok_or(eagle_core::ProviderError::ApiChanged)?;
                if count.is_some_and(|count| count != tracks.len() as u64) {
                    return Err(eagle_core::ProviderError::ApiChanged);
                }
            }
            None if count == Some(0) => {}
            None => return Err(eagle_core::ProviderError::ApiChanged),
        }
        let playlist = self.parse_playlist(&value)?;
        if playlist.id != *id {
            return Err(eagle_core::ProviderError::ApiChanged);
        }
        let revision = value["revision"]
            .as_u64()
            .ok_or(eagle_core::ProviderError::ApiChanged)?;
        Ok(PlaylistSnapshot { playlist, revision })
    }

    /// Experimental: never automatically retries an uncertain write.
    pub fn edit_playlist_unverified(
        &self,
        snapshot: &PlaylistSnapshot,
        edit: PlaylistEdit,
    ) -> Result<PlaylistSnapshot, PlaylistEditError> {
        use eagle_core::ProviderError;
        let invalid = || PlaylistEditError::Provider(ProviderError::InvalidId);
        let (owner, kind) = self
            .local(&snapshot.playlist.id)
            .map_err(PlaylistEditError::Provider)?
            .split_once('_')
            .ok_or_else(invalid)?;
        if owner.is_empty()
            || kind.is_empty()
            || !owner.bytes().all(|b| b.is_ascii_digit())
            || !kind.bytes().all(|b| b.is_ascii_digit())
            || owner != self.uid().map_err(PlaylistEditError::Provider)?
        {
            return Err(invalid());
        }
        let mut expected: Vec<_> = snapshot
            .playlist
            .tracks
            .iter()
            .map(|track| (track.id.clone(), track.album.clone()))
            .collect();
        let diff = match edit {
            PlaylistEdit::Insert { at, tracks }
                if at <= snapshot.playlist.tracks.len() && !tracks.is_empty() =>
            {
                expected.splice(
                    at..at,
                    tracks
                        .iter()
                        .map(|track| (track.track_id.clone(), Some(track.album_id.clone()))),
                );
                let tracks = tracks
                    .iter()
                    .map(|track| {
                        let id = self.local(&track.track_id)?;
                        let album = self.local(&track.album_id)?;
                        if id.is_empty()
                            || album.is_empty()
                            || !id.bytes().all(|b| b.is_ascii_digit())
                            || !album.bytes().all(|b| b.is_ascii_digit())
                        {
                            return Err(ProviderError::InvalidId);
                        }
                        Ok(serde_json::json!({"id": id, "albumId": album}))
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(PlaylistEditError::Provider)?;
                serde_json::json!([{"op":"insert", "at":at, "tracks":tracks}])
            }
            PlaylistEdit::Delete { from, to }
                if from < to && to <= snapshot.playlist.tracks.len() =>
            {
                expected.drain(from..to);
                serde_json::json!([{"op":"delete", "from":from, "to":to}])
            }
            PlaylistEdit::Move { from, to }
                if from < snapshot.playlist.tracks.len()
                    && to < snapshot.playlist.tracks.len()
                    && from != to =>
            {
                let track = &snapshot.playlist.tracks[from];
                let id = self.local(&track.id).map_err(PlaylistEditError::Provider)?;
                let album = track.album.as_ref().ok_or_else(invalid)?;
                let album = self.local(album).map_err(PlaylistEditError::Provider)?;
                if id.is_empty()
                    || album.is_empty()
                    || !id.bytes().all(|b| b.is_ascii_digit())
                    || !album.bytes().all(|b| b.is_ascii_digit())
                {
                    return Err(invalid());
                }
                let moved = expected.remove(from);
                expected.insert(to, moved);
                serde_json::json!([
                    {"op":"delete", "from":from, "to":from + 1},
                    {"op":"insert", "at":to, "tracks":[{"id":id, "albumId":album}]}
                ])
            }
            _ => return Err(invalid()),
        };
        let current = self
            .playlist_snapshot_unverified(&snapshot.playlist.id)
            .map_err(PlaylistEditError::Provider)?;
        if current.revision != snapshot.revision {
            return Err(PlaylistEditError::Conflict);
        }
        let result = self
            .api(
                &format!("/users/{owner}/playlists/{kind}/change"),
                Some(vec![
                    ("kind".into(), kind.into()),
                    ("revision".into(), snapshot.revision.to_string()),
                    ("diff".into(), diff.to_string()),
                ]),
            )
            .map_err(|_| PlaylistEditError::Uncertain)?;
        let playlist = self
            .parse_playlist(&result)
            .map_err(|_| PlaylistEditError::Uncertain)?;
        let revision = result["revision"]
            .as_u64()
            .ok_or(PlaylistEditError::Uncertain)?;
        if playlist.id != snapshot.playlist.id || revision <= snapshot.revision {
            return Err(PlaylistEditError::Uncertain);
        }
        let verified = self
            .playlist_snapshot_unverified(&snapshot.playlist.id)
            .map_err(|_| PlaylistEditError::Uncertain)?;
        if verified.revision != revision
            || verified
                .playlist
                .tracks
                .iter()
                .map(|track| (&track.id, &track.album))
                .ne(expected.iter().map(|(id, album)| (id, album)))
        {
            return Err(PlaylistEditError::Uncertain);
        }
        Ok(verified)
    }
}
