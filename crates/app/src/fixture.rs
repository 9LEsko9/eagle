// SPDX-License-Identifier: GPL-3.0-or-later
//! Deterministic desktop fixture: no credentials and no external requests.
use eagle_core::*;
use eagle_store::{Cache, RangeMap, Store};
use std::{path::Path, sync::Arc};

pub fn provider() -> Arc<dyn Provider> {
    Arc::new(Fixture)
}
struct Fixture;
fn tracks() -> Vec<Track> {
    (1..=100)
        .filter_map(|n| {
            Some(Track {
                id: format!("fixture:{n}").parse().ok()?,
                title: format!("Fixture track {n:03}"),
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
/// Seeds an isolated managed root with 100 complete local files.
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
    let provider = provider();
    store.put_setting(
        &crate::providers::account_key(provider.info().id),
        "fixture".into(),
    )?;
    for track in tracks() {
        store.put_track(&track)?;
        let mut map = RangeMap::new(wav.len() as u64);
        cache.write_chunk(&store, &track.id, Quality::High, &mut map, 0, &wav)?;
        cache.pin(&store, &track.id, Quality::High, true)?;
        store.record_audio_owner("fixture", &track.id.to_string())?;
        store.enqueue_download("fixture", &track.id, Quality::High)?;
    }
    while let Some(job) = store.claim_download("fixture")? {
        store.finish_saved_download(&job)?;
    }
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
            auth: AuthMethod::Token {
                hint: "",
                help_url: None,
            },
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
            subscription: true,
            capabilities: capabilities(),
        })
    }
    fn search(&self, _: &str, page: u32) -> Result<Page<Track>, ProviderError> {
        self.likes((page > 0).then_some("end"))
    }
    fn track(&self, id: &TrackId) -> Result<Track, ProviderError> {
        tracks()
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
            items: if cursor.is_none() { tracks() } else { vec![] },
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
