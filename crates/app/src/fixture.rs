// SPDX-License-Identifier: GPL-3.0-or-later
//! Deterministic desktop fixture: no credentials and no external requests.
use eagle_core::*;
use eagle_store::{Cache, RangeMap, Store};
use std::{path::Path, sync::Arc};

pub fn provider() -> Arc<dyn Provider> {
    Arc::new(Fixture(100))
}
pub fn stress_provider() -> Arc<dyn Provider> {
    Arc::new(Fixture(10_000))
}
struct Fixture(usize);
fn tracks(count: usize) -> Vec<Track> {
    (1..=count)
        .filter_map(|n| {
            Some(Track {
                id: format!("fixture:{n}").parse().ok()?,
                title: format!("Трек Track 曲 {n:05}"),
                artists: vec![],
                album: None,
                album_title: None,
                duration_ms: 30_000,
                cover: None,
                availability: Availability::Available,
                isrc: None,
            })
        })
        .collect()
}
/// Prepares stress metadata, a durable queue and 1,000 offline jobs before measurement.
pub fn seed(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let store = Store::open(root)?;
    let cache = Cache::open(&root.join("audio"), 16 * 1024 * 1024, &store)?;
    let samples: Vec<u8> = (0..30 * 8000)
        .flat_map(|n| {
            let sample = ((n as f32 * 440. * std::f32::consts::TAU / 8000.).sin() * 1500.) as i16;
            sample.to_le_bytes()
        })
        .collect();
    let mut wav = Vec::new();
    wav.extend(b"RIFF");
    wav.extend((36 + samples.len() as u32).to_le_bytes());
    wav.extend(b"WAVEfmt ");
    wav.extend(16u32.to_le_bytes());
    wav.extend(1u16.to_le_bytes());
    wav.extend(1u16.to_le_bytes());
    wav.extend(8000u32.to_le_bytes());
    wav.extend(16000u32.to_le_bytes());
    wav.extend(2u16.to_le_bytes());
    wav.extend(16u16.to_le_bytes());
    wav.extend(b"data");
    wav.extend((samples.len() as u32).to_le_bytes());
    wav.extend(samples);
    let provider = stress_provider();
    store.put_setting(
        &crate::providers::account_key(provider.info().id),
        "fixture".into(),
    )?;
    let tracks = tracks(10_000);
    store.put_tracks(&tracks)?;
    store.sync_provider_likes(
        "fixture",
        &tracks.iter().map(|t| t.id.clone()).collect::<Vec<_>>(),
    )?;
    let albums = (1..=1_000)
        .map(|n| -> Result<Album, Box<dyn std::error::Error>> {
            Ok(Album {
                id: format!("fixture:album{n}").parse()?,
                title: format!("Альбом Album {n:04}"),
                artists: vec![],
                tracks: vec![],
                cover: None,
                year: Some(2026),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    store.put_setting(
        &crate::providers::liked_albums_key("fixture"),
        serde_json::to_string(&albums)?,
    )?;
    let profile = store.profile_id()?;
    store.save_playback(
        &profile,
        eagle_store::PlaybackState {
            entries: tracks
                .iter()
                .enumerate()
                .map(|(n, track)| eagle_store::PlaybackEntry {
                    id: n as u64 + 1,
                    track: track.clone(),
                    radio: None,
                })
                .collect(),
            order: (1..=tracks.len() as u64).collect(),
            current: Some(1),
            ..Default::default()
        },
    )?;
    for track in tracks.iter().take(1_000) {
        let mut map = RangeMap::new(wav.len() as u64);
        cache.write_chunk(&store, &track.id, Quality::High, &mut map, 0, &wav)?;
        cache.pin(&store, &track.id, Quality::High, true)?;
        store.record_audio_owner("fixture", &track.id.to_string())?;
        store.enqueue_download("fixture", &track.id, Quality::High)?;
    }
    while let Some(job) = store.claim_download("fixture")? {
        store.finish_saved_download(&job)?;
    }
    std::fs::write(root.join(".fixture-prepared"), b"stress-v1")?;
    Ok(())
}
/// What the fixture implements; capability-driven routing reaches only these methods.
const fn capabilities() -> Capabilities {
    let mut c = Capabilities::none();
    c.search_tracks = true;
    c.likes = true;
    c.playlists_read = true;
    c
}
impl Provider for Fixture {
    fn info(&self) -> &ProviderInfo {
        static INFO: ProviderInfo = ProviderInfo {
            id: "fixture",
            name: "fixture",
            sign_in: &[SignInMethod::Token {
                hint: "",
                help_url: None,
            }],
            capabilities: capabilities(),
        };
        &INFO
    }
    fn login(&self, _: Token) -> Result<Account, ProviderError> {
        self.account()
    }
    fn account(&self) -> Result<Account, ProviderError> {
        Ok(Account {
            id: "fixture".into(),
            name: None,
            subscription: true,
            capabilities: capabilities(),
        })
    }
    fn search(&self, _: &str, page: u32) -> Result<Page<Track>, ProviderError> {
        self.likes((page > 0).then_some("end"))
    }
    fn track(&self, id: &TrackId) -> Result<Track, ProviderError> {
        tracks(self.0)
            .into_iter()
            .find(|t| t.id == *id)
            .ok_or(ProviderError::NotFound)
    }
    fn album(&self, _: &TrackId) -> Result<Album, ProviderError> {
        Err(ProviderError::NotFound)
    }
    fn playlist(&self, _: &TrackId) -> Result<Playlist, ProviderError> {
        Err(ProviderError::NotFound)
    }
    fn playlists(&self, _: Option<&str>) -> Result<Page<Playlist>, ProviderError> {
        Ok(Page {
            items: vec![],
            next: None,
        })
    }
    fn likes(&self, cursor: Option<&str>) -> Result<Page<Track>, ProviderError> {
        Ok(Page {
            items: if cursor.is_none() {
                tracks(self.0)
            } else {
                vec![]
            },
            next: None,
        })
    }
    fn set_liked(&self, _: &TrackId, _: bool) -> Result<(), ProviderError> {
        Ok(())
    }
    fn set_disliked(&self, _: &TrackId, _: bool) -> Result<(), ProviderError> {
        Ok(())
    }
    fn resolve_stream(&self, _: &TrackId, _: Quality) -> Result<Stream, ProviderError> {
        Err(ProviderError::Network)
    }
}
